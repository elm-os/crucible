// Package core is the runtime shared by the public packages: the process-wide client, its span processor, its
// feedback sender and the values a context carries to every span.
package core

import (
	"context"
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"sync"
	"sync/atomic"
	"time"

	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/exporters/otlp/otlptrace/otlptracehttp"
	"go.opentelemetry.io/otel/sdk/resource"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
	"go.opentelemetry.io/otel/trace"
	"go.opentelemetry.io/otel/trace/noop"
)

// Version is the SDK version, sent in the User-Agent of every export request.
const Version = "0.1.1"

// ScopeName is the instrumentation scope of every span the SDK starts.
const ScopeName = "github.com/elm-os/crucible/sdk/go"

// Resource keys (contracts 3.1, 9.1, 9.7, 10.6).
const (
	keyServiceName    = "service.name"
	keyServiceVersion = "service.version"
	keyInstanceID     = "service.instance.id"
	keyEnvironment    = "deployment.environment.name"
	keyDeployID       = "crucible.deploy.id"
)

// Config is a validated configuration.
type Config struct {
	TracesURL      string
	SecretKey      string
	ServiceName    string
	ServiceVersion string
	Environment    string
	Release        string
	DeployID       string
	QueueSize      int
	QueueBytes     int64
	BatchSize      int
	FlushInterval  time.Duration
	// FeedbackURL is the project's feedback route; empty without a project id.
	FeedbackURL string
}

// Client is one configured exporter, span processor, tracer and feedback sender.
type Client struct {
	provider  *sdktrace.TracerProvider
	tracer    trace.Tracer
	processor *Processor
	feedback  *FeedbackSender
	secretKey []byte
}

// New builds a client. It does not install it.
func New(cfg Config) (*Client, error) {
	exporter, err := otlptracehttp.New(context.Background(),
		otlptracehttp.WithEndpointURL(cfg.TracesURL),
		otlptracehttp.WithHeaders(map[string]string{
			"Authorization": "Bearer " + cfg.SecretKey,
			"User-Agent":    "crucible-sdk-go/" + Version,
		}),
	)
	if err != nil {
		return nil, err
	}
	res, err := newResource(cfg)
	if err != nil {
		return nil, err
	}
	processor := NewProcessor(exporter, cfg.Release, cfg.QueueSize, cfg.QueueBytes, cfg.BatchSize, cfg.FlushInterval)
	provider := sdktrace.NewTracerProvider(
		// Every span is kept: a remote parent that was not sampled must not suppress an MCP operation.
		sdktrace.WithSampler(sdktrace.AlwaysSample()),
		sdktrace.WithResource(res),
		sdktrace.WithSpanProcessor(processor),
	)
	processors.Store(trace.TracerProvider(provider), processor)
	return &Client{
		provider:  provider,
		tracer:    provider.Tracer(ScopeName, trace.WithInstrumentationVersion(Version)),
		processor: processor,
		feedback:  NewFeedbackSender(cfg.FeedbackURL, cfg.SecretKey, processor.ForceFlush),
		secretKey: []byte(cfg.SecretKey),
	}, nil
}

func newResource(cfg Config) (*resource.Resource, error) {
	var kv []attribute.KeyValue
	add := func(k, v string) {
		if v != "" {
			kv = append(kv, attribute.String(k, v))
		}
	}
	add(keyServiceName, cfg.ServiceName)
	add(keyServiceVersion, cfg.ServiceVersion)
	add(keyEnvironment, cfg.Environment)
	add(keyDeployID, cfg.DeployID)
	if _, ok := resource.Default().Set().Value(keyInstanceID); !ok {
		add(keyInstanceID, randomID())
	}
	return resource.Merge(resource.Default(), resource.NewSchemaless(kv...))
}

// randomID returns a random version 4 UUID: the process's `service.instance.id` (contract 9.4, rule 4).
func randomID() string {
	var b [16]byte
	_, _ = rand.Read(b[:])
	b[6] = b[6]&0x0f | 0x40
	b[8] = b[8]&0x3f | 0x80
	return fmt.Sprintf("%x-%x-%x-%x-%x", b[0:4], b[4:6], b[6:8], b[8:10], b[10:16])
}

// Tracer returns the client's tracer.
func (c *Client) Tracer() trace.Tracer { return c.tracer }

// Flush exports every span that has ended and sends every queued score. It returns the export error, else the
// feedback sender's.
func (c *Client) Flush(ctx context.Context) error {
	err := c.processor.ForceFlush(ctx)
	if ferr := c.feedback.Flush(ctx); err == nil {
		err = ferr
	}
	return err
}

// Shutdown flushes and stops the client: the spans first, then the scores.
func (c *Client) Shutdown(ctx context.Context) error {
	err := c.provider.Shutdown(ctx)
	processors.Delete(trace.TracerProvider(c.provider))
	if ferr := c.feedback.Shutdown(ctx); err == nil {
		err = ferr
	}
	return err
}

// Feedback queues one score for the feedback route.
func (c *Client) Feedback(body FeedbackBody, key string) { c.feedback.Add(body, key) }

// Stats returns the processor's and the feedback sender's counters.
func (c *Client) Stats() Stats {
	s := c.processor.Stats()
	s.FeedbackSent, s.FeedbackDropped, s.FeedbackFailed = c.feedback.Counts()
	return s
}

var active atomic.Pointer[Client]

// processors maps each client's tracer provider to its processor, so that the work a span defers reaches the
// processor that exports the span, even when another client was installed while the span ran.
var processors sync.Map

// Defer attaches work to span, to run on the export path once span has ended (see Deferred). Without a client the
// span records nothing, and the work is dropped.
func Defer(span trace.Span, work Deferred) {
	if p, ok := processors.Load(span.TracerProvider()); ok {
		p.(*Processor).Defer(span.SpanContext().SpanID(), work)
	}
}

// Install makes c the process-wide client. It reports false when another client is installed.
func Install(c *Client) bool { return active.CompareAndSwap(nil, c) }

// Uninstall removes c if it is the process-wide client.
func Uninstall(c *Client) { active.CompareAndSwap(c, nil) }

// Active returns the process-wide client, or nil.
func Active() *Client { return active.Load() }

var noopTracer = noop.NewTracerProvider().Tracer(ScopeName)

// Tracer returns the process-wide client's tracer, or a tracer that records nothing before Init.
func Tracer() trace.Tracer {
	if c := active.Load(); c != nil {
		return c.tracer
	}
	return noopTracer
}

// StateHash returns the keyed hash of a multi-round requestState (contract 9.6): the first 16 bytes of
// HMAC-SHA256 keyed with the secret key over "crucible.mrtr.v1", a zero byte and the state, as 32 hex
// characters. It returns "" before Init.
func StateHash(state string) string {
	c := active.Load()
	if c == nil {
		return ""
	}
	return stateHash(c.secretKey, state)
}

func stateHash(key []byte, state string) string {
	m := hmac.New(sha256.New, key)
	m.Write([]byte("crucible.mrtr.v1"))
	m.Write([]byte{0})
	m.Write([]byte(state))
	return hex.EncodeToString(m.Sum(nil)[:16])
}

// SessionKey returns the hashed form of a session id that `mcp.session.id` carries (contract 9.4): the first
// 16 bytes of SHA-256 of the raw id, as 32 hex characters.
func SessionKey(id string) string {
	sum := sha256.Sum256([]byte(id))
	return hex.EncodeToString(sum[:16])
}
