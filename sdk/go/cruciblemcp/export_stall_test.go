package cruciblemcp_test

// The stalled-collector scenario of the shared SDK suite (conformance/export-stall.json, contract 9.10): with
// capture on and a collector that never answers, the export queue stays under its byte ceiling and drops.

import (
	"context"
	"encoding/json"
	"os"
	"testing"
	"time"

	"github.com/elm-os/crucible/sdk/go/crucible"
	"github.com/elm-os/crucible/sdk/go/cruciblemcp"
	"github.com/elm-os/crucible/sdk/go/internal/collectortest"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

type stallScenario struct {
	Schema      string `json:"schema"`
	Tool        string `json:"tool"`
	MaxBytes    int    `json:"max_bytes"`
	ResultBytes int    `json:"result_bytes"`
	Calls       int    `json:"calls"`
	QueueSpans  int    `json:"queue_spans"`
	QueueBytes  int64  `json:"queue_bytes"`
}

func TestAStalledCollectorKeepsTheQueueUnderItsByteCeiling(t *testing.T) {
	data, err := os.ReadFile("../../../conformance/export-stall.json")
	if err != nil {
		t.Skipf("the SDK is checked out without conformance/: %v", err)
	}
	var s stallScenario
	if err := json.Unmarshal(data, &s); err != nil {
		t.Fatal(err)
	}
	if s.Schema != "crucible.conformance.export-stall/v1" || s.Calls >= s.QueueSpans {
		t.Fatalf("scenario %+v: schema or call count this test does not read", s)
	}
	prose := loadOverhead(t).Fills["prose"]
	text := repeatTo(prose, s.ResultBytes)
	// Registered first so it runs last: the stalled collector is released before the SDK shuts down.
	t.Cleanup(func() {
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = crucible.Shutdown(ctx)
	})
	c := collectortest.NewStalled(t)
	if err := crucible.Init(crucible.Options{Endpoint: c.URL, SecretKey: testKey, FlushInterval: time.Minute}); err != nil {
		t.Fatal(err)
	}
	result := &mcp.CallToolResult{Content: []mcp.Content{&mcp.TextContent{Text: text}}}
	cs := shapeSession(t, s.Tool, map[string]*mcp.CallToolResult{"": result}, cruciblemcp.Options{
		Capture: []cruciblemcp.CaptureRule{{Tool: s.Tool, Args: true, Result: true, MaxBytes: s.MaxBytes}},
	})
	var most int64
	args := &mcp.CallToolParams{Name: s.Tool, Arguments: map[string]any{"shape": ""}}
	for range s.Calls {
		if _, err := cs.CallTool(t.Context(), args); err != nil {
			t.Fatal(err)
		}
		most = max(most, crucible.CurrentStats().QueuedBytes)
	}
	dropped := crucible.CurrentStats().QueueFull
	t.Logf("export-stall go: most queued %d bytes (ceiling %d), dropped %d", most, s.QueueBytes, dropped)
	if most > s.QueueBytes {
		t.Errorf("bytes queued: %d > %d", most, s.QueueBytes)
	}
	if dropped == 0 {
		t.Errorf("no span was dropped")
	}
}
