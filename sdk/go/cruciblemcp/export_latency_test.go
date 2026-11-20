package cruciblemcp_test

// A slow or down collector adds no latency to the caller (conformance/export-latency.json, contract 9.10): the
// same calls run against a collector that answers, one that never answers and a port where nothing listens. A
// call that waited for an export would take the export timeout; every call stays far below it, and Flush and
// Shutdown keep to their deadlines. The added p99 over the healthy run is logged, and gated only on the laptop
// rig (CRUCIBLE_CAPTURE_GATE=1).

import (
	"context"
	"encoding/json"
	"net"
	"os"
	"slices"
	"testing"
	"time"

	"github.com/elm-os/crucible/sdk/go/crucible"
	"github.com/elm-os/crucible/sdk/go/cruciblemcp"
	"github.com/elm-os/crucible/sdk/go/internal/collectortest"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

type latencyScenario struct {
	Schema          string   `json:"schema"`
	Tool            string   `json:"tool"`
	MaxBytes        int      `json:"max_bytes"`
	ResultBytes     int      `json:"result_bytes"`
	Collectors      []string `json:"collectors"`
	Warmup          int      `json:"warmup"`
	Calls           int      `json:"calls"`
	ExportTimeoutMS int      `json:"export_timeout_ms"`
	CallCeilingMS   float64  `json:"call_ceiling_ms"`
	SlackMS         int      `json:"slack_ms"`
	GateAddedP99MS  float64  `json:"gate_added_p99_ms"`
}

// endpointIn returns the endpoint of a collector in state.
func endpointIn(t *testing.T, state string) string {
	t.Helper()
	switch state {
	case "healthy":
		return collectortest.New(t).URL
	case "stalled":
		return collectortest.NewStalled(t).URL
	}
	// A port that was free a moment ago: nothing listens on it.
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	addr := l.Addr().String()
	_ = l.Close()
	return "http://" + addr
}

func TestAStalledOrRefusingCollectorAddsNoLatencyToCalls(t *testing.T) {
	data, err := os.ReadFile("../../../conformance/export-latency.json")
	if err != nil {
		t.Skipf("the SDK is checked out without conformance/: %v", err)
	}
	var s latencyScenario
	if err := json.Unmarshal(data, &s); err != nil {
		t.Fatal(err)
	}
	if s.Schema != "crucible.conformance.export-latency/v1" || s.Calls <= 512 {
		t.Fatalf("scenario %+v: schema or call count this test does not read", s)
	}
	text := repeatTo(loadOverhead(t).Fills["prose"], s.ResultBytes)
	deadline := time.Duration(s.ExportTimeoutMS) * time.Millisecond
	slack := time.Duration(s.SlackMS) * time.Millisecond
	p99s := map[string]float64{}
	for _, state := range s.Collectors {
		t.Run(state, func(t *testing.T) {
			if err := crucible.Init(crucible.Options{Endpoint: endpointIn(t, state), SecretKey: testKey}); err != nil {
				t.Fatal(err)
			}
			stopped := false
			t.Cleanup(func() {
				if !stopped {
					_ = crucible.Shutdown(context.Background())
				}
			})
			result := &mcp.CallToolResult{Content: []mcp.Content{&mcp.TextContent{Text: text}}}
			cs := shapeSession(t, s.Tool, map[string]*mcp.CallToolResult{"": result}, cruciblemcp.Options{
				Capture: []cruciblemcp.CaptureRule{{Tool: s.Tool, Args: true, Result: true, MaxBytes: s.MaxBytes}},
			})
			args := &mcp.CallToolParams{Name: s.Tool, Arguments: map[string]any{"shape": ""}}
			for range s.Warmup {
				if _, err := cs.CallTool(t.Context(), args); err != nil {
					t.Fatal(err)
				}
			}
			samples := make([]float64, 0, s.Calls)
			for range s.Calls {
				start := time.Now()
				if _, err := cs.CallTool(t.Context(), args); err != nil {
					t.Fatal(err)
				}
				samples = append(samples, float64(time.Since(start).Microseconds())/1000)
			}
			ctx, cancel := context.WithTimeout(context.Background(), deadline)
			start := time.Now()
			_ = crucible.Flush(ctx)
			flushed := time.Since(start)
			cancel()
			ctx, cancel = context.WithTimeout(context.Background(), deadline)
			start = time.Now()
			_ = crucible.Shutdown(ctx)
			shut := time.Since(start)
			cancel()
			stopped = true
			p99s[state] = quantile(samples, 0.99)
			slowest := slices.Max(samples)
			t.Logf("export-latency go %s: p50 %.3f ms, p99 %.3f ms, slowest %.3f ms, flush %v, shutdown %v",
				state, quantile(samples, 0.5), p99s[state], slowest, flushed.Round(time.Millisecond), shut.Round(time.Millisecond))
			if state != "healthy" && slowest > s.CallCeilingMS {
				t.Errorf("%s: a call took %.1f ms, more than %.0f ms", state, slowest, s.CallCeilingMS)
			}
			if flushed > deadline+slack || shut > deadline+slack {
				t.Errorf("%s: flush took %v and shutdown %v, more than %v", state, flushed, shut, deadline+slack)
			}
		})
	}
	for _, state := range []string{"stalled", "refused"} {
		added := p99s[state] - p99s["healthy"]
		t.Logf("export-latency go %s: added p99 %.3f ms over the healthy collector", state, added)
		if os.Getenv("CRUCIBLE_CAPTURE_GATE") == "1" && !(added <= s.GateAddedP99MS) {
			t.Errorf("%s: added p99 %.3f ms > %.3f ms", state, added, s.GateAddedP99MS)
		}
	}
}
