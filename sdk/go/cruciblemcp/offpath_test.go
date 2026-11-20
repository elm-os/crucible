package cruciblemcp_test

// Content capture off the request path (contract 9.10): the window of a captured field is taken while the
// request runs, and the scrubbing rules, the Scrub option and the error message's scrubbing run on the export
// path after the span ends, so a slow scrubber never delays the caller.

import (
	"context"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/elm-os/crucible/sdk/go/crucible"
	"github.com/elm-os/crucible/sdk/go/cruciblemcp"
	tracepb "go.opentelemetry.io/proto/otlp/trace/v1"
)

func TestACapturedCallReturnsWhileTheScrubHookWaits(t *testing.T) {
	c := startCrucible(t)
	release := make(chan struct{})
	var scrubbed atomic.Int32
	opts := cruciblemcp.Options{
		Capture: []cruciblemcp.CaptureRule{{Tool: "*", Args: true, Result: true}},
		Scrub: func(field, value string) (string, bool) {
			scrubbed.Add(1)
			<-release
			if field == cruciblemcp.FieldResult {
				return "", false
			}
			return strings.ToUpper(value), true
		},
	}
	cs := connect(t, newServer(t, opts), newClient(t, nil), modern)

	returned := make(chan error, 2)
	go func() {
		_, err := call(t, cs, "echo", map[string]any{"text": "reach ada@example.com"})
		returned <- err
	}()
	go func() {
		_, err := call(t, cs, "fail", nil)
		returned <- err
	}()
	for range 2 {
		select {
		case err := <-returned:
			if err != nil {
				t.Fatal(err)
			}
		case <-time.After(5 * time.Second):
			close(release)
			t.Fatalf("a captured call did not return while the Scrub hook waited (%d hook calls): scrubbing runs on the request path", scrubbed.Load())
		}
	}
	close(release)

	echo := server(t, c, "tools/call echo")
	expect(t, echo, map[string]any{
		"gen_ai.tool.call.arguments":      `{"TEXT":"REACH [REDACTED:EMAIL]"}`,
		"gen_ai.tool.call.result":         nil,
		"crucible.mcp.content.captured":   true,
		"crucible.mcp.content.truncation": "none",
	})
	fail := server(t, c, "tools/call fail")
	if fail.StatusCode != tracepb.Status_STATUS_CODE_ERROR || fail.StatusMessage != "THE WAREHOUSE SERVICE IS DOWN" {
		t.Errorf("fail: status %v %q, want an error with the message the Scrub hook returned", fail.StatusCode, fail.StatusMessage)
	}
}

func TestShutdownFinishesTheCaptureOfEveryQueuedSpan(t *testing.T) {
	c := startCrucible(t)
	cs := connect(t, newServer(t, cruciblemcp.Options{
		Capture: []cruciblemcp.CaptureRule{{Tool: "secret", Args: true, Result: true}},
	}), newClient(t, nil), legacy)
	for range 3 {
		if _, err := call(t, cs, "secret", map[string]any{"note": "key csk_0123456789abcdefghijABCDEFGHIJ0123456789abc"}); err != nil {
			t.Fatal(err)
		}
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := crucible.Shutdown(ctx); err != nil {
		t.Fatal(err)
	}
	var captured int
	for _, s := range c.Spans() {
		if s.Name != "tools/call secret" || s.Kind != tracepb.Span_SPAN_KIND_SERVER {
			continue
		}
		captured++
		expect(t, s, map[string]any{"gen_ai.tool.call.arguments": `{"note":"key [redacted:secret_key]"}`})
		if r, _ := s.Attrs["gen_ai.tool.call.result"].(string); !strings.Contains(r, "[redacted:email]") || strings.Contains(r, "ada@") {
			t.Errorf("result %q: not scrubbed", r)
		}
	}
	if captured != 3 {
		t.Fatalf("%d captured spans exported at shutdown, want 3", captured)
	}
}
