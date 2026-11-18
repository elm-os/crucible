package crucible_test

import (
	"context"
	"strings"
	"testing"
	"time"

	"github.com/elm-os/crucible/sdk/go/crucible"
	"github.com/elm-os/crucible/sdk/go/internal/collectortest"
	tracepb "go.opentelemetry.io/proto/otlp/trace/v1"
)

const testKey = "csk_0123456789abcdefghijABCDEFGHIJ0123456789abc"

func start(t *testing.T, c *collectortest.Collector, opts crucible.Options) {
	t.Helper()
	opts.Endpoint = c.URL
	if opts.SecretKey == "" {
		opts.SecretKey = testKey
	}
	if err := crucible.Init(opts); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = crucible.Shutdown(context.Background()) })
}

func find(t *testing.T, spans []collectortest.Span, name string) collectortest.Span {
	t.Helper()
	for _, s := range spans {
		if s.Name == name {
			return s
		}
	}
	t.Fatalf("no span named %q among %d spans", name, len(spans))
	return collectortest.Span{}
}

func TestInitRefusalsSayWhatToChange(t *testing.T) {
	const ok = "http://localhost:4318"
	for _, tc := range []struct {
		opts crucible.Options
		want string
	}{
		{crucible.Options{SecretKey: testKey}, "Options.Endpoint is empty; set it to the base URL"},
		{crucible.Options{Endpoint: "localhost:4318", SecretKey: testKey}, "is not an http or https URL; set it to"},
		{crucible.Options{Endpoint: "ftp://host", SecretKey: testKey}, "is not an http or https URL"},
		{crucible.Options{Endpoint: ok}, "Options.SecretKey is empty; set it to a project secret key"},
		{crucible.Options{Endpoint: ok, SecretKey: "cpk_abc"}, "not a public key (cpk_...)"},
		{crucible.Options{Endpoint: ok, SecretKey: testKey, QueueSize: -1}, "set it to 0 for the default of 2048"},
		{crucible.Options{Endpoint: ok, SecretKey: testKey, FlushInterval: -time.Second}, "set it to 0 for the default"},
	} {
		err := crucible.Init(tc.opts)
		if err == nil {
			_ = crucible.Shutdown(context.Background())
			t.Fatalf("Init(%+v) succeeded", tc.opts)
		}
		if !strings.Contains(err.Error(), tc.want) {
			t.Errorf("Init(%+v) = %q, want it to say %q", tc.opts, err, tc.want)
		}
	}
}

func TestInitTwiceIsRefusedUntilShutdown(t *testing.T) {
	c := collectortest.New(t)
	opts := crucible.Options{Endpoint: c.URL, SecretKey: testKey}
	if err := crucible.Init(opts); err != nil {
		t.Fatal(err)
	}
	if err := crucible.Init(opts); err == nil || !strings.Contains(err.Error(), "call crucible.Shutdown before calling Init again") {
		t.Fatalf("second Init = %v", err)
	}
	if err := crucible.Shutdown(context.Background()); err != nil {
		t.Fatal(err)
	}
	if err := crucible.Init(opts); err != nil {
		t.Fatalf("Init after Shutdown = %v", err)
	}
	_ = crucible.Shutdown(context.Background())
}

func TestObservationsCarryTheSourceKeys(t *testing.T) {
	c := collectortest.New(t)
	start(t, c, crucible.Options{
		ServiceName:    "support-agent",
		ServiceVersion: "1.4.0",
		Environment:    "staging",
		Release:        "r42",
		DeployID:       "deploy-7",
	})

	ctx := crucible.WithSession(context.Background(), "session-1")
	ctx = crucible.WithUser(ctx, "user-1")
	ctx = crucible.WithTags(ctx, "beta", "checkout")
	ctx = crucible.WithTags(ctx, "beta", "eu")
	ctx = crucible.WithMetadata(ctx, "plan", "pro")
	ctx = crucible.WithMetadata(ctx, "seats", 3)
	ctx = crucible.WithMetadata(ctx, "seats", 4)
	ctx = crucible.WithMetadata(ctx, "trial", false)
	ctx = crucible.WithMetadata(ctx, "limits", map[string]int{"rpm": 60})
	ctx, run := crucible.Observe(ctx, "answer", crucible.Agent)
	_, gen := crucible.Observe(ctx, "draft", crucible.Generation)
	gen.SetModel("model-a")
	gen.SetInput(map[string]any{"question": "where is my order?"})
	gen.SetOutput("It ships today.")
	gen.SetUsage(crucible.Usage{Input: 120, Output: 30, CacheRead: 100, Reasoning: 10})
	gen.SetCost(crucible.Cost{Total: 0.0021, Input: 0.0012})
	gen.End()
	run.SetError(context.DeadlineExceeded)
	run.End()
	if err := crucible.Flush(context.Background()); err != nil {
		t.Fatal(err)
	}

	spans := c.Spans()
	draft, answer := find(t, spans, "draft"), find(t, spans, "answer")
	want := map[string]any{
		"crucible.observation.type":            "generation",
		"gen_ai.request.model":                 "model-a",
		"crucible.input":                       `{"question":"where is my order?"}`,
		"crucible.output":                      "It ships today.",
		"gen_ai.usage.input_tokens":            int64(120),
		"gen_ai.usage.output_tokens":           int64(30),
		"gen_ai.usage.cache_read.input_tokens": int64(100),
		"gen_ai.usage.reasoning.output_tokens": int64(10),
		"crucible.cost.total":                  0.0021,
		"crucible.cost.input":                  0.0012,
		"crucible.session.id":                  "session-1",
		"crucible.user.id":                     "user-1",
		"crucible.release":                     "r42",
		"crucible.metadata.plan":               "pro",
		"crucible.metadata.seats":              int64(4),
		"crucible.metadata.trial":              false,
		"crucible.metadata.limits":             `{"rpm":60}`,
	}
	for k, v := range want {
		if draft.Attrs[k] != v {
			t.Errorf("draft %s = %#v, want %#v", k, draft.Attrs[k], v)
		}
	}
	for _, k := range []string{"gen_ai.usage.cache_creation.input_tokens", "crucible.cost.output"} {
		if _, ok := draft.Attrs[k]; ok {
			t.Errorf("draft has %s, a zero field", k)
		}
	}
	if got := strings.Join(draft.Attrs["crucible.tags"].([]string), ","); got != "beta,checkout,eu" {
		t.Errorf("tags = %q", got)
	}
	if answer.Attrs["crucible.session.id"] != "session-1" || answer.Attrs["crucible.observation.type"] != "agent" {
		t.Errorf("answer attributes = %v", answer.Attrs)
	}
	if draft.ParentSpanID != answer.SpanID || draft.TraceID != answer.TraceID {
		t.Errorf("draft is not a child of answer: %+v %+v", draft, answer)
	}
	if answer.StatusCode != tracepb.Status_STATUS_CODE_ERROR || answer.StatusMessage != context.DeadlineExceeded.Error() {
		t.Errorf("answer status = %v %q", answer.StatusCode, answer.StatusMessage)
	}
	if draft.Scope != "github.com/elm-os/crucible/sdk/go" {
		t.Errorf("scope = %q", draft.Scope)
	}
	for k, v := range map[string]any{
		"service.name":                "support-agent",
		"service.version":             "1.4.0",
		"deployment.environment.name": "staging",
		"crucible.deploy.id":          "deploy-7",
		"telemetry.sdk.language":      "go",
	} {
		if draft.Resource[k] != v {
			t.Errorf("resource %s = %#v, want %#v", k, draft.Resource[k], v)
		}
	}
	if id, _ := draft.Resource["service.instance.id"].(string); len(id) != 36 {
		t.Errorf("service.instance.id = %q, want a UUID", id)
	}

	reqs := c.Requests()
	if len(reqs) == 0 {
		t.Fatal("no export request")
	}
	for _, r := range reqs {
		if r.Method != "POST" || r.Path != "/v1/traces" || r.ContentType != "application/x-protobuf" {
			t.Errorf("request %s %s %s", r.Method, r.Path, r.ContentType)
		}
		if r.Authorization != "Bearer "+testKey {
			t.Errorf("Authorization = %q", r.Authorization)
		}
		if r.UserAgent != "crucible-sdk-go/"+crucible.Version {
			t.Errorf("User-Agent = %q", r.UserAgent)
		}
	}
	if s := crucible.CurrentStats(); s.Exported != 2 || s.QueueFull != 0 || s.ExportFailed != 0 {
		t.Errorf("stats = %+v", s)
	}
}

func TestBeforeInitNothingIsRecorded(t *testing.T) {
	_, o := crucible.Observe(context.Background(), "idle", crucible.Span)
	if o.Span().IsRecording() {
		t.Fatal("an observation records before Init")
	}
	o.End()
	if err := crucible.Flush(context.Background()); err != nil {
		t.Fatal(err)
	}
}

// A collector that never answers cannot slow the caller: a full queue drops and counts, and Shutdown returns
// by its deadline.
func TestStalledCollectorNeverBlocks(t *testing.T) {
	c := collectortest.NewStalled(t)
	if err := crucible.Init(crucible.Options{Endpoint: c.URL, SecretKey: testKey, QueueSize: 8, FlushInterval: 5 * time.Millisecond}); err != nil {
		t.Fatal(err)
	}
	began := time.Now()
	for i := 0; i < 5000; i++ {
		_, o := crucible.Observe(context.Background(), "step", crucible.Span)
		o.End()
	}
	if d := time.Since(began); d > 5*time.Second {
		t.Errorf("5000 observations took %v", d)
	}
	if s := crucible.CurrentStats(); s.QueueFull == 0 {
		t.Errorf("stats = %+v, want spans dropped for a full queue", s)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 300*time.Millisecond)
	defer cancel()
	began = time.Now()
	if err := crucible.Shutdown(ctx); err == nil {
		t.Error("Shutdown against a stalled collector reported success")
	}
	if d := time.Since(began); d > 5*time.Second {
		t.Errorf("Shutdown took %v with a 300ms deadline", d)
	}
}

// A refused export is counted and returned by Flush.
func TestRefusedExportIsCountedAndReturned(t *testing.T) {
	c := collectortest.New(t)
	c.Close()
	if err := crucible.Init(crucible.Options{Endpoint: c.URL, SecretKey: testKey}); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = crucible.Shutdown(context.Background()) })
	_, o := crucible.Observe(context.Background(), "step", crucible.Span)
	o.End()
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	if err := crucible.Flush(ctx); err == nil {
		t.Fatal("Flush to a closed collector reported success")
	}
	if s := crucible.CurrentStats(); s.ExportFailed != 1 {
		t.Errorf("stats = %+v", s)
	}
}
