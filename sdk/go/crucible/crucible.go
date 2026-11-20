package crucible

import (
	"context"
	"errors"
	"fmt"
	"net/url"
	"strings"
	"time"

	"github.com/elm-os/crucible/sdk/go/internal/core"
	"go.opentelemetry.io/otel/attribute"
)

// Options configures the process-wide client.
type Options struct {
	// Endpoint is the base URL of the Crucible server, for example http://localhost:4318. Spans are sent to
	// Endpoint + "/v1/traces". Plain http works.
	Endpoint string
	// SecretKey is a project secret key (csk_...), sent as a Bearer token. The ingest scope sends spans and the
	// write scope feedback; new keys have both.
	SecretKey string
	// ProjectID is the id of the key's project (a UUID), which SendFeedback needs; crucible admin create-key prints
	// it as project_id. Without it, scores are dropped and counted.
	ProjectID string
	// APIURL is the base URL of the Crucible API listener, where SendFeedback posts. When empty it is Endpoint. The
	// API listener (port 8080) also takes spans; the OTLP listener (port 4318) does not serve the API.
	APIURL string

	// ServiceName and ServiceVersion become the resource's service.name and service.version. An MCP
	// server's name and version default to them.
	ServiceName    string
	ServiceVersion string
	// Environment becomes the resource's deployment.environment.name.
	Environment string
	// Release is set on every span as crucible.release. When empty, Crucible uses ServiceVersion.
	Release string
	// DeployID becomes the resource's crucible.deploy.id. When empty, Crucible uses the server version.
	DeployID string

	// QueueSize is how many finished spans wait for export; when the queue is full, new spans are dropped and
	// counted in Stats. 0 means 2048.
	QueueSize int
	// QueueBytes bounds the bytes of the spans waiting for export and of the batch being exported, as estimated
	// from their names, attributes, events and status messages; beyond it new spans are dropped and counted in
	// Stats too. 0 means 8 MiB.
	QueueBytes int
	// FlushInterval is how often queued spans are exported. 0 means one second.
	FlushInterval time.Duration
}

const (
	defaultQueueSize     = 2048
	defaultQueueBytes    = 8 << 20 // the proxy's telemetry buffer (contract 9.10)
	defaultBatchSize     = 512
	defaultFlushInterval = time.Second
)

// Init configures the process-wide client from opts and starts its exporter. It returns an error that says
// which option to change when opts is incomplete, and when Init was already called.
func Init(opts Options) error {
	cfg, err := validate(opts)
	if err != nil {
		return err
	}
	c, err := core.New(cfg)
	if err != nil {
		return fmt.Errorf("crucible: starting the exporter: %w", err)
	}
	if !core.Install(c) {
		_ = c.Shutdown(context.Background())
		return errors.New("crucible: Init was already called; call crucible.Shutdown before calling Init again")
	}
	return nil
}

func validate(opts Options) (core.Config, error) {
	const example = "set it to the base URL of your Crucible server, for example http://localhost:4318"
	if opts.Endpoint == "" {
		return core.Config{}, errors.New("crucible: Options.Endpoint is empty; " + example)
	}
	u, err := url.Parse(opts.Endpoint)
	if err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" {
		return core.Config{}, fmt.Errorf("crucible: Options.Endpoint %q is not an http or https URL; %s", opts.Endpoint, example)
	}
	if opts.SecretKey == "" {
		return core.Config{}, errors.New("crucible: Options.SecretKey is empty; set it to a project secret key (csk_...) with the ingest scope")
	}
	if !strings.HasPrefix(opts.SecretKey, "csk_") {
		return core.Config{}, errors.New("crucible: Options.SecretKey does not start with csk_; sending telemetry needs a project secret key, not a public key (cpk_...)")
	}
	feedbackURL, err := feedbackRoute(opts)
	if err != nil {
		return core.Config{}, err
	}
	if opts.QueueSize < 0 {
		return core.Config{}, errors.New("crucible: Options.QueueSize is negative; set it to 0 for the default of 2048")
	}
	if opts.QueueBytes < 0 {
		return core.Config{}, errors.New("crucible: Options.QueueBytes is negative; set it to 0 for the default of 8 MiB")
	}
	if opts.FlushInterval < 0 {
		return core.Config{}, errors.New("crucible: Options.FlushInterval is negative; set it to 0 for the default of one second")
	}
	cfg := core.Config{
		TracesURL:      strings.TrimRight(opts.Endpoint, "/") + "/v1/traces",
		SecretKey:      opts.SecretKey,
		ServiceName:    opts.ServiceName,
		ServiceVersion: opts.ServiceVersion,
		Environment:    opts.Environment,
		Release:        opts.Release,
		DeployID:       opts.DeployID,
		QueueSize:      opts.QueueSize,
		QueueBytes:     int64(opts.QueueBytes),
		BatchSize:      defaultBatchSize,
		FlushInterval:  opts.FlushInterval,
		FeedbackURL:    feedbackURL,
	}
	if cfg.QueueSize == 0 {
		cfg.QueueSize = defaultQueueSize
	}
	if cfg.QueueBytes == 0 {
		cfg.QueueBytes = defaultQueueBytes
	}
	if cfg.FlushInterval == 0 {
		cfg.FlushInterval = defaultFlushInterval
	}
	return cfg, nil
}

// Flush exports every span that has ended and sends every queued score. It returns the export error, if any, or
// ctx's error when ctx ends first. Before Init it does nothing.
func Flush(ctx context.Context) error {
	if c := core.Active(); c != nil {
		return c.Flush(ctx)
	}
	return nil
}

// Shutdown flushes, stops the client and lets Init be called again. When ctx ends first, the spans and scores
// still queued are dropped.
func Shutdown(ctx context.Context) error {
	c := core.Active()
	if c == nil {
		return nil
	}
	err := c.Shutdown(ctx)
	core.Uninstall(c)
	return err
}

// Stats counts what happened to finished spans and to scores since Init.
type Stats struct {
	// Exported is the number of spans the server accepted.
	Exported uint64
	// QueueFull is the number of spans dropped because the export queue was full.
	QueueFull uint64
	// ExportFailed is the number of spans dropped because their export failed.
	ExportFailed uint64
	// QueuedBytes is the estimated size of the spans waiting for export and of the batch being exported.
	QueuedBytes int64
	// FeedbackSent is the number of scores the server accepted.
	FeedbackSent uint64
	// FeedbackDropped is the number of scores never sent: the feedback queue was full, Options.ProjectID is empty,
	// or Shutdown ran out of time before them.
	FeedbackDropped uint64
	// FeedbackFailed is the number of scores the server refused, or that failed on every attempt.
	FeedbackFailed uint64
}

// CurrentStats returns the counters of the process-wide client; zero before Init.
func CurrentStats() Stats {
	c := core.Active()
	if c == nil {
		return Stats{}
	}
	s := c.Stats()
	return Stats{
		Exported: s.Exported, QueueFull: s.QueueFull, ExportFailed: s.ExportFailed, QueuedBytes: s.QueuedBytes,
		FeedbackSent: s.FeedbackSent, FeedbackDropped: s.FeedbackDropped, FeedbackFailed: s.FeedbackFailed,
	}
}

// WithSession returns a context whose spans carry the session id (crucible.session.id).
func WithSession(ctx context.Context, id string) context.Context { return core.WithSession(ctx, id) }

// WithUser returns a context whose spans carry the user id (crucible.user.id).
func WithUser(ctx context.Context, id string) context.Context { return core.WithUser(ctx, id) }

// WithTags returns a context whose spans carry the tags (crucible.tags), added to those ctx carries.
func WithTags(ctx context.Context, tags ...string) context.Context {
	return core.WithTags(ctx, tags...)
}

// WithMetadata returns a context whose spans carry one metadata entry (crucible.metadata.<key>). A string, bool,
// integer or float is sent as that type; any other value as JSON text.
func WithMetadata(ctx context.Context, key string, value any) context.Context {
	return core.WithMetadata(ctx, key, metadataValue(value))
}

func metadataValue(v any) attribute.Value {
	switch x := v.(type) {
	case string:
		return attribute.StringValue(x)
	case bool:
		return attribute.BoolValue(x)
	case int:
		return attribute.IntValue(x)
	case int32:
		return attribute.Int64Value(int64(x))
	case int64:
		return attribute.Int64Value(x)
	case float32:
		return attribute.Float64Value(float64(x))
	case float64:
		return attribute.Float64Value(x)
	}
	return attribute.StringValue(encode(v))
}
