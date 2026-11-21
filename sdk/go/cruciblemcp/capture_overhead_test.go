package cruciblemcp_test

// The capture-overhead scenario of the shared SDK suite (conformance/capture-overhead.json, contract 9.10): the
// same tool with capture off and on, in paired alternating rounds over fixed result shapes. The work hook bounds
// what the capture path serializes and scrubs per field; the added p99 is logged, and gated only on the laptop
// rig (CRUCIBLE_CAPTURE_GATE=1).

import (
	"context"
	"encoding/json"
	"fmt"
	"math"
	"os"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/elm-os/crucible/sdk/go/crucible"
	"github.com/elm-os/crucible/sdk/go/cruciblemcp"
	"github.com/elm-os/crucible/sdk/go/internal/collectortest"
	"github.com/elm-os/crucible/sdk/go/internal/scrub"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

type overheadRun struct {
	Warmup int `json:"warmup"`
	Rounds int `json:"rounds"`
	Calls  int `json:"calls"`
}

type overheadBlock struct {
	Type     string `json:"type"`
	Fill     string `json:"fill"`
	Length   int    `json:"length"`
	MIMEType string `json:"mime_type"`
}

type overheadShape struct {
	ID      string          `json:"id"`
	Blocks  []overheadBlock `json:"blocks"`
	Records int             `json:"records"`
	// GateRelativeP99 is the share of the uncaptured p99 the added p99 may reach, when that is more than the
	// absolute gate.
	GateRelativeP99 float64 `json:"gate_relative_p99"`
}

type overheadScenario struct {
	Schema    string `json:"schema"`
	Tool      string `json:"tool"`
	MaxBytes  int    `json:"max_bytes"`
	WorkBound int    `json:"work_bound"`
	// MatcherStepsPerRuleChar bounds matcher steps per captured call, per rule and byte scrubbed.
	MatcherStepsPerRuleChar int                    `json:"matcher_steps_per_rule_char"`
	CeilingMS               float64                `json:"ceiling_ms"`
	GateAddedP99MS          float64                `json:"gate_added_p99_ms"`
	Runs                    map[string]overheadRun `json:"runs"`
	Fills                   map[string]string      `json:"fills"`
	Shapes                  []overheadShape        `json:"shapes"`
}

func loadOverhead(t *testing.T) overheadScenario {
	t.Helper()
	data, err := os.ReadFile("../../../conformance/capture-overhead.json")
	if err != nil {
		t.Skipf("the SDK is checked out without conformance/: %v", err)
	}
	var s overheadScenario
	if err := json.Unmarshal(data, &s); err != nil {
		t.Fatal(err)
	}
	if s.Schema != "crucible.conformance.capture-overhead/v1" {
		t.Fatalf("schema %q; this test reads crucible.conformance.capture-overhead/v1", s.Schema)
	}
	return s
}

func repeatTo(unit string, n int) string {
	return strings.Repeat(unit, n/len(unit)+1)[:n]
}

// shapeResult builds a shape. Image data is held decoded, as the Go MCP library holds it; it marshals to the
// scenario's base64 fill.
func shapeResult(s overheadScenario, shape overheadShape) *mcp.CallToolResult {
	r := &mcp.CallToolResult{}
	for _, b := range shape.Blocks {
		if b.Type == "image" {
			data := make([]byte, b.Length/4*3)
			for i := range data {
				data[i] = byte(i % 256)
			}
			r.Content = append(r.Content, &mcp.ImageContent{Data: data, MIMEType: b.MIMEType})
			continue
		}
		r.Content = append(r.Content, &mcp.TextContent{Text: repeatTo(s.Fills[b.Fill], b.Length)})
	}
	if shape.Records > 0 {
		rows := make([]any, shape.Records)
		for i := range rows {
			id := i + 1
			rows[i] = map[string]any{"id": id, "sku": fmt.Sprintf("SKU-%05d", id), "qty": id % 7}
		}
		r.StructuredContent = map[string]any{"records": rows}
	}
	return r
}

// shapeSession serves the shape tool from a server instrumented with opts and connects a client to it.
func shapeSession(t *testing.T, tool string, results map[string]*mcp.CallToolResult, opts cruciblemcp.Options) *mcp.ClientSession {
	t.Helper()
	s := mcp.NewServer(&mcp.Implementation{Name: "capture-overhead", Version: "1.0.0"}, nil)
	in, err := cruciblemcp.Instrument(s, opts)
	if err != nil {
		t.Fatal(err)
	}
	s.AddTool(&mcp.Tool{Name: tool, InputSchema: map[string]any{"type": "object"}},
		func(_ context.Context, req *mcp.CallToolRequest) (*mcp.CallToolResult, error) {
			var args struct {
				Shape string `json:"shape"`
			}
			if err := json.Unmarshal(req.Params.Arguments, &args); err != nil {
				return nil, err
			}
			return results[args.Shape], nil
		})
	st, ct := mcp.NewInMemoryTransports()
	ss, err := s.Connect(t.Context(), in.Transport(st), nil)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = ss.Close() })
	c := mcp.NewClient(&mcp.Implementation{Name: "capture-overhead-client", Version: "1.0.0"}, nil)
	cs, err := c.Connect(t.Context(), ct, &mcp.ClientSessionOptions{ProtocolVersion: modern})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = cs.Close() })
	return cs
}

func quantile(samples []float64, q float64) float64 {
	sorted := slices.Sorted(slices.Values(samples))
	return sorted[min(len(sorted)-1, int(float64(len(sorted))*q))]
}

func p99(samples []float64) float64 { return quantile(samples, 0.99) }

func TestCaptureOverhead(t *testing.T) {
	s := loadOverhead(t)
	gated := os.Getenv("CRUCIBLE_CAPTURE_GATE") == "1"
	run := s.Runs["ci"]
	if gated {
		run = s.Runs["laptop"]
	}
	// Registered first so it runs last: the collector is still up while the SDK shuts down.
	t.Cleanup(func() {
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = crucible.Shutdown(ctx)
	})
	// A collector that answers, as in production: the scrubbing that captured calls leave for the export path
	// runs on the export goroutine while the rounds run, and its cost to the calls is part of what is timed.
	c := collectortest.New(t)
	if err := crucible.Init(crucible.Options{Endpoint: c.URL, SecretKey: testKey, FlushInterval: time.Minute}); err != nil {
		t.Fatal(err)
	}
	results := map[string]*mcp.CallToolResult{}
	for _, shape := range s.Shapes {
		results[shape.ID] = shapeResult(s, shape)
	}
	off := shapeSession(t, s.Tool, results, cruciblemcp.Options{})
	on := shapeSession(t, s.Tool, results, cruciblemcp.Options{
		Capture: []cruciblemcp.CaptureRule{{Tool: s.Tool, Args: true, Result: true, MaxBytes: s.MaxBytes}},
	})
	for _, shape := range s.Shapes {
		t.Run(shape.ID, func(t *testing.T) {
			args := &mcp.CallToolParams{Name: s.Tool, Arguments: map[string]any{"shape": shape.ID}}
			call := func(cs *mcp.ClientSession) float64 {
				start := time.Now()
				if _, err := cs.CallTool(t.Context(), args); err != nil {
					t.Fatal(err)
				}
				return float64(time.Since(start).Microseconds()) / 1000
			}
			flush := func() {
				if err := crucible.Flush(t.Context()); err != nil {
					t.Fatal(err)
				}
			}
			for range run.Warmup {
				call(off)
				call(on)
			}
			flush()
			// The work of one captured call, with the hooks installed; the timed rounds run without them. The
			// scrubbing runs on the export goroutine, and the second flush waits for it while the hooks are in.
			var mu sync.Mutex
			serialized, scrubbed, scrubbedInCall, steps := 0, 0, 0, 0
			cruciblemcp.SetWorkHook(func(sz, sc int) {
				mu.Lock()
				defer mu.Unlock()
				serialized, scrubbed, scrubbedInCall = max(serialized, sz), max(scrubbed, sc), scrubbedInCall+sc
			})
			scrub.SetMatcherHook(func(_ string, n int) {
				mu.Lock()
				defer mu.Unlock()
				steps += n
			})
			call(on)
			flush()
			cruciblemcp.SetWorkHook(nil)
			scrub.SetMatcherHook(nil)
			if steps == 0 {
				t.Fatalf("the scrubber's matcher took no steps for the captured call: its scrubbing did not run")
			}
			stepsPerChar := float64(steps) / float64(max(scrubbedInCall, 1))
			var without, with []float64
			for round := range run.Rounds {
				arms := []*mcp.ClientSession{off, on}
				if round%2 == 1 {
					arms = []*mcp.ClientSession{on, off}
				}
				for _, arm := range arms {
					for range run.Calls {
						if arm == on {
							with = append(with, call(arm))
						} else {
							without = append(without, call(arm))
						}
					}
				}
			}
			added := p99(with) - p99(without)
			gate := max(s.GateAddedP99MS, shape.GateRelativeP99*p99(without))
			excess := slices.Max(with) - quantile(without, 0.5)
			t.Logf("capture-overhead go %s: added p99 %.3f ms (off %.3f, on %.3f, gate %.3f), slowest excess %.3f ms, serialized %d, scrubbed %d, matcher steps %d (%.1f per character)",
				shape.ID, added, p99(without), p99(with), gate, excess, serialized, scrubbed, steps, stepsPerChar)
			if serialized > s.WorkBound {
				t.Errorf("bytes serialized for one field: %d > %d", serialized, s.WorkBound)
			}
			if scrubbed > s.WorkBound {
				t.Errorf("bytes handed to the scrubber for one field: %d > %d", scrubbed, s.WorkBound)
			}
			if bound := float64(s.MatcherStepsPerRuleChar * scrub.RuleCount()); stepsPerChar > bound {
				t.Errorf("matcher steps per byte scrubbed: %.1f > %.0f", stepsPerChar, bound)
			}
			if excess > s.CeilingMS {
				t.Errorf("slowest captured call over the median uncaptured one: %.1f ms > %.0f ms", excess, s.CeilingMS)
			}
			if gated && !(added <= gate) {
				t.Errorf("added p99: %.3f ms > %.3f ms", added, gate)
			}
			if math.IsNaN(added) {
				t.Errorf("no samples")
			}
		})
	}
}
