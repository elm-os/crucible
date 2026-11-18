package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"strconv"

	"github.com/elm-os/crucible/sdk/go/crucible"
	"go.opentelemetry.io/otel/attribute"
)

// node is one span of an agent case and the fields set on it.
type node struct {
	Name           string            `json:"name"`
	Type           string            `json:"type"`
	TraceName      string            `json:"trace_name"`
	Context        *nodeContext      `json:"context"`
	Input          json.RawMessage   `json:"input"`
	Output         json.RawMessage   `json:"output"`
	Model          string            `json:"model"`
	Provider       string            `json:"provider"`
	InputMessages  json.RawMessage   `json:"input_messages"`
	OutputMessages json.RawMessage   `json:"output_messages"`
	FinishReasons  []string          `json:"finish_reasons"`
	Usage          map[string]int64  `json:"usage"`
	CostUSD        map[string]string `json:"cost_usd"`
	Children       []node            `json:"children"`
}

type nodeContext struct {
	SessionID string                     `json:"session_id"`
	UserID    string                     `json:"user_id"`
	Tags      []string                   `json:"tags"`
	Metadata  map[string]json.RawMessage `json:"metadata"`
}

func runAgent(spec runSpec) error {
	if err := initSDK(spec); err != nil {
		return err
	}
	ctx := context.Background()
	shut := false
	for _, st := range spec.Steps {
		switch {
		case st.Observe != nil:
			if err := observe(ctx, *st.Observe); err != nil {
				return fmt.Errorf("step %s: %w", st.Step, err)
			}
		case st.Flush != nil:
			if err := crucible.Flush(ctx); err != nil {
				return fmt.Errorf("step %s: %w", st.Step, err)
			}
		case st.Checkpoint != "":
			if err := checkpoint(spec.CheckpointURL, st.Checkpoint); err != nil {
				return fmt.Errorf("step %s: %w", st.Step, err)
			}
		case st.Shutdown != nil:
			if err := crucible.Shutdown(ctx); err != nil {
				return fmt.Errorf("step %s: %w", st.Step, err)
			}
			shut = true
		default:
			return fmt.Errorf("step %s: not an agent step", st.Step)
		}
	}
	if shut {
		return nil
	}
	return crucible.Shutdown(ctx)
}

// observe starts a span for n with the SDK's helpers, runs its children inside it and ends it. Keys the SDK has no
// setter for (trace name, provider, messages, finish reasons) go through Observation.Span.
func observe(ctx context.Context, n node) error {
	if c := n.Context; c != nil {
		if c.SessionID != "" {
			ctx = crucible.WithSession(ctx, c.SessionID)
		}
		if c.UserID != "" {
			ctx = crucible.WithUser(ctx, c.UserID)
		}
		ctx = crucible.WithTags(ctx, c.Tags...)
		for k, raw := range c.Metadata {
			v, err := scalar(raw)
			if err != nil {
				return fmt.Errorf("metadata %s: %w", k, err)
			}
			ctx = crucible.WithMetadata(ctx, k, v)
		}
	}
	ctx, o := crucible.Observe(ctx, n.Name, crucible.ObservationType(n.Type))
	defer o.End()
	span := o.Span()
	if n.TraceName != "" {
		span.SetAttributes(attribute.String("crucible.trace.name", n.TraceName))
	}
	if n.Input != nil {
		o.SetInput(textOrJSON(n.Input))
	}
	if n.Output != nil {
		o.SetOutput(textOrJSON(n.Output))
	}
	if n.Model != "" {
		o.SetModel(n.Model)
	}
	if n.Provider != "" {
		span.SetAttributes(attribute.String("gen_ai.provider.name", n.Provider))
	}
	if n.InputMessages != nil {
		span.SetAttributes(attribute.String("gen_ai.input.messages", compact(n.InputMessages)))
	}
	if n.OutputMessages != nil {
		span.SetAttributes(attribute.String("gen_ai.output.messages", compact(n.OutputMessages)))
	}
	if len(n.FinishReasons) > 0 {
		span.SetAttributes(attribute.StringSlice("gen_ai.response.finish_reasons", n.FinishReasons))
	}
	if len(n.Usage) > 0 {
		o.SetUsage(crucible.Usage{
			Input:      n.Usage["input_tokens"],
			Output:     n.Usage["output_tokens"],
			CacheRead:  n.Usage["cache_read.input_tokens"],
			CacheWrite: n.Usage["cache_creation.input_tokens"],
			Reasoning:  n.Usage["reasoning.output_tokens"],
		})
	}
	if len(n.CostUSD) > 0 {
		c, err := cost(n.CostUSD)
		if err != nil {
			return err
		}
		o.SetCost(c)
	}
	for _, child := range n.Children {
		if err := observe(ctx, child); err != nil {
			return err
		}
	}
	return nil
}

func cost(usd map[string]string) (crucible.Cost, error) {
	var c crucible.Cost
	fields := map[string]*float64{
		"total": &c.Total, "input": &c.Input, "output": &c.Output,
		"cache_read": &c.CacheRead, "cache_write": &c.CacheWrite, "reasoning": &c.Reasoning,
	}
	for k, v := range usd {
		f, ok := fields[k]
		if !ok {
			return c, fmt.Errorf("cost key %q has no field", k)
		}
		x, err := strconv.ParseFloat(v, 64)
		if err != nil {
			return c, fmt.Errorf("cost %s: %w", k, err)
		}
		*f = x
	}
	return c, nil
}

// textOrJSON is a JSON string's text, sent verbatim, or any other JSON value's text.
func textOrJSON(raw json.RawMessage) any {
	var s string
	if json.Unmarshal(raw, &s) == nil {
		return s
	}
	return json.RawMessage(compact(raw))
}

// scalar decodes a metadata value: a string, a bool, an integer or a float; anything else stays JSON.
func scalar(raw json.RawMessage) (any, error) {
	d := json.NewDecoder(bytes.NewReader(raw))
	d.UseNumber()
	var v any
	if err := d.Decode(&v); err != nil {
		return nil, err
	}
	if n, ok := v.(json.Number); ok {
		if i, err := n.Int64(); err == nil {
			return i, nil
		}
		return n.Float64()
	}
	return v, nil
}

func compact(raw json.RawMessage) string {
	var b bytes.Buffer
	if json.Compact(&b, raw) != nil {
		return string(raw)
	}
	return b.String()
}

// checkpoint asks the collector to order a named checkpoint against the exports so far.
func checkpoint(url, name string) error {
	body, err := json.Marshal(map[string]string{"name": name})
	if err != nil {
		return err
	}
	resp, err := http.Post(url, "application/json", bytes.NewReader(body))
	if err != nil {
		return err
	}
	_ = resp.Body.Close()
	if resp.StatusCode != http.StatusNoContent {
		return fmt.Errorf("checkpoint %s: status %d", name, resp.StatusCode)
	}
	return nil
}
