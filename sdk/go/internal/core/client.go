// Package core is the runtime shared by the public packages: the process-wide client, its span processor and
// the values a context carries to every span.
package core

import (
	"context"
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
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
const Version = "0.1.0"

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
}

// Client is one configured exporter, span processor and tracer.
type Client struct {
	provider  *sdktrace.TracerProvider
	tracer    trace.Tracer
	processor *Processor
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
	return &Client{
		provider:  provider,
		tracer:    provider.Tracer(ScopeName, trace.WithInstrumentationVersion(Version)),
		processor: processor,
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

// Flush exports every span that has ended.
func (c *Client) Flush(ctx context.Context) error { return c.processor.ForceFlush(ctx) }

// Shutdown flushes and stops the client.
func (c *Client) Shutdown(ctx context.Context) error { return c.provider.Shutdown(ctx) }

// Stats returns the processor's counters.
func (c *Client) Stats() Stats { return c.processor.Stats() }

var active atomic.Pointer[Client]

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
