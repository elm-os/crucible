package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"time"

	"github.com/elm-os/crucible/sdk/go/crucible"
	"github.com/elm-os/crucible/sdk/go/cruciblemcp"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

// requestState is the state the needs_input tool hands back, fixed by the reference server contract.
const requestState = "crucible-conformance-request-state-1"

func text(s string) *mcp.CallToolResult {
	return &mcp.CallToolResult{Content: []mcp.Content{&mcp.TextContent{Text: s}}}
}

// newReferenceServer builds the reference server of the runner contract on the official MCP Go SDK and
// instruments it with the run spec's options.
func newReferenceServer(spec runSpec) (*mcp.Server, *cruciblemcp.Instrumentation, error) {
	s := mcp.NewServer(&mcp.Implementation{Name: spec.Server.Name, Version: spec.Server.Version}, nil)
	mcp.AddTool(s, &mcp.Tool{Name: "echo", Description: "Returns its text."},
		func(_ context.Context, _ *mcp.CallToolRequest, in struct {
			Text string `json:"text"`
		}) (*mcp.CallToolResult, any, error) {
			return text(in.Text), nil, nil
		})
	mcp.AddTool(s, &mcp.Tool{Name: "fail", Description: "Fails with the reason, annotated as an upstream error."},
		func(ctx context.Context, _ *mcp.CallToolRequest, in struct {
			Reason string `json:"reason"`
		}) (*mcp.CallToolResult, any, error) {
			cruciblemcp.AnnotateError(ctx, cruciblemcp.SubclassUpstream)
			return nil, nil, errors.New(in.Reason)
		})
	mcp.AddTool(s, &mcp.Tool{Name: "throw", Description: "Returns an error, Go's exception path."},
		func(_ context.Context, _ *mcp.CallToolRequest, in struct {
			Message string `json:"message"`
		}) (*mcp.CallToolResult, any, error) {
			return nil, nil, errors.New(in.Message)
		})
	mcp.AddTool(s, &mcp.Tool{Name: "slow", Description: "Waits, sending progress when asked."}, slow)
	mcp.AddTool(s, &mcp.Tool{Name: "secret", Description: "Returns its note unchanged."},
		func(_ context.Context, _ *mcp.CallToolRequest, in struct {
			Note string `json:"note"`
		}) (*mcp.CallToolResult, any, error) {
			return text(in.Note), nil, nil
		})
	mcp.AddTool(s, &mcp.Tool{Name: "needs_input", Description: "Asks for a confirmation before answering."}, needsInput)
	mcp.AddTool(s, &mcp.Tool{Name: "lookup_order", Description: "Looks an order up."},
		func(_ context.Context, _ *mcp.CallToolRequest, in struct {
			OrderID string `json:"order_id"`
		}) (*mcp.CallToolResult, any, error) {
			return text(fmt.Sprintf("order %s: shipped", in.OrderID)), nil, nil
		})

	capture := make([]cruciblemcp.CaptureRule, 0, len(spec.SDK.Capture))
	for _, r := range spec.SDK.Capture {
		capture = append(capture, cruciblemcp.CaptureRule{
			Tool: r.Tool, Args: r.Args, Result: r.Result, SampleRate: r.SampleRate, MaxBytes: r.MaxBytes,
		})
	}
	in, err := cruciblemcp.Instrument(s, cruciblemcp.Options{
		ServerName:    spec.Server.Name,
		ServerVersion: spec.Server.Version,
		Capture:       capture,
		HandleParams:  spec.SDK.HandleParams,
	})
	return s, in, err
}

type slowIn struct {
	MS    int `json:"ms"`
	Steps int `json:"steps"`
}

// slow waits ms milliseconds in all, stopping early when cancelled. When the request carries a progress token
// it sends steps progress notifications spread over the wait.
func slow(ctx context.Context, req *mcp.CallToolRequest, in slowIn) (*mcp.CallToolResult, any, error) {
	total := time.Duration(in.MS) * time.Millisecond
	token := req.Params.GetProgressToken()
	steps := in.Steps
	if token == nil {
		steps = 0
	}
	start := time.Now()
	for i := 1; i <= steps; i++ {
		if err := sleepUntil(ctx, start.Add(total*time.Duration(i)/time.Duration(steps+1))); err != nil {
			return nil, nil, err
		}
		p := &mcp.ProgressNotificationParams{ProgressToken: token, Progress: float64(i), Total: float64(steps)}
		if err := req.Session.NotifyProgress(ctx, p); err != nil {
			return nil, nil, err
		}
	}
	if err := sleepUntil(ctx, start.Add(total)); err != nil {
		return nil, nil, err
	}
	return text("done"), nil, nil
}

func sleepUntil(ctx context.Context, t time.Time) error {
	timer := time.NewTimer(time.Until(t))
	defer timer.Stop()
	select {
	case <-timer.C:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

type needsInputIn struct {
	Item string `json:"item"`
}

// needsInput answers input_required with a form elicitation until it is retried with the state and an accepted
// confirmation.
func needsInput(_ context.Context, req *mcp.CallToolRequest, in needsInputIn) (*mcp.CallToolResult, any, error) {
	if req.Params.RequestState == "" {
		return &mcp.CallToolResult{
			InputRequests: mcp.InputRequestMap{"confirm": &mcp.ElicitParams{
				Message: "Confirm " + in.Item + "?",
				RequestedSchema: map[string]any{
					"type":       "object",
					"properties": map[string]any{"confirmed": map[string]any{"type": "boolean"}},
					"required":   []string{"confirmed"},
				},
			}},
			RequestState: requestState,
		}, nil, nil
	}
	if r, ok := req.Params.InputResponses["confirm"].(*mcp.ElicitResult); !ok || r.Action != "accept" {
		return nil, nil, errors.New("the confirmation was not accepted")
	}
	return text("confirmed " + in.Item), nil, nil
}

// serveStdio is the stdio reference server: it serves on stdin and stdout until stdin closes, then flushes.
func serveStdio(specJSON string) error {
	var spec runSpec
	if err := json.Unmarshal([]byte(specJSON), &spec); err != nil {
		return err
	}
	if err := initSDK(spec); err != nil {
		return err
	}
	s, in, err := newReferenceServer(spec)
	if err != nil {
		return err
	}
	if err := s.Run(context.Background(), in.Transport(&mcp.StdioTransport{})); err != nil {
		fmt.Fprintln(os.Stderr, "reference server stopped:", err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	return crucible.Shutdown(ctx)
}
