package crucible

import (
	"context"
	"encoding/json"
	"fmt"

	"github.com/elm-os/crucible/sdk/go/internal/core"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/codes"
	"go.opentelemetry.io/otel/trace"
)

// ObservationType is the kind of step an observation records (crucible.observation.type).
type ObservationType string

// The observation types.
const (
	Span       ObservationType = "span"
	Generation ObservationType = "generation"
	Event      ObservationType = "event"
	Embedding  ObservationType = "embedding"
	Agent      ObservationType = "agent"
	Tool       ObservationType = "tool"
	Chain      ObservationType = "chain"
	Retriever  ObservationType = "retriever"
	Evaluator  ObservationType = "evaluator"
	Guardrail  ObservationType = "guardrail"
)

// Span keys of contract 10.6 and the GenAI semantic conventions.
const (
	keyObservationType = "crucible.observation.type"
	keyInput           = "crucible.input"
	keyOutput          = "crucible.output"
	keyCostPrefix      = "crucible.cost."
	keyRequestModel    = "gen_ai.request.model"
	keyInputTokens     = "gen_ai.usage.input_tokens"
	keyOutputTokens    = "gen_ai.usage.output_tokens"
	keyCacheReadTokens = "gen_ai.usage.cache_read.input_tokens"
	keyCacheWrite      = "gen_ai.usage.cache_creation.input_tokens"
	keyReasoningTokens = "gen_ai.usage.reasoning.output_tokens"
)

// Observation is one step of a trace: a span that ends when End is called.
type Observation struct {
	span trace.Span
}

// Observe starts an observation named name as a child of the span in ctx, and returns a context that carries
// it. Before Init the observation records nothing.
func Observe(ctx context.Context, name string, typ ObservationType) (context.Context, *Observation) {
	var opts []trace.SpanStartOption
	if typ != "" {
		opts = append(opts, trace.WithAttributes(attribute.String(keyObservationType, string(typ))))
	}
	ctx, span := core.Tracer().Start(ctx, name, opts...)
	return ctx, &Observation{span: span}
}

// Span returns the underlying OpenTelemetry span, for attributes this package does not set.
func (o *Observation) Span() trace.Span { return o.span }

// SetInput records the input (crucible.input). A string or []byte is sent as is; anything else as JSON.
func (o *Observation) SetInput(v any) { o.span.SetAttributes(attribute.String(keyInput, encode(v))) }

// SetOutput records the output (crucible.output). A string or []byte is sent as is; anything else as JSON.
func (o *Observation) SetOutput(v any) { o.span.SetAttributes(attribute.String(keyOutput, encode(v))) }

// SetModel records the model a generation requested (gen_ai.request.model), which prices its usage.
func (o *Observation) SetModel(model string) {
	o.span.SetAttributes(attribute.String(keyRequestModel, model))
}

// Usage is the token usage of a generation. CacheRead and CacheWrite are parts of Input, and Reasoning is a
// part of Output, as in the GenAI semantic conventions. Zero fields are not sent.
type Usage struct {
	Input, Output, CacheRead, CacheWrite, Reasoning int64
}

// SetUsage records token usage (gen_ai.usage.*). Crucible prices it when no cost is set.
func (o *Observation) SetUsage(u Usage) {
	for _, f := range []struct {
		key string
		n   int64
	}{
		{keyInputTokens, u.Input},
		{keyOutputTokens, u.Output},
		{keyCacheReadTokens, u.CacheRead},
		{keyCacheWrite, u.CacheWrite},
		{keyReasoningTokens, u.Reasoning},
	} {
		if f.n != 0 {
			o.span.SetAttributes(attribute.Int64(f.key, f.n))
		}
	}
}

// Cost is a cost in USD that the caller already knows. Total is the whole cost; the other fields are the
// cost of one usage key each. Zero fields are not sent.
type Cost struct {
	Total, Input, Output, CacheRead, CacheWrite, Reasoning float64
}

// SetCost records a provided cost (crucible.cost.total and crucible.cost.<usage key>). A provided cost
// replaces the cost Crucible would calculate from the usage.
func (o *Observation) SetCost(c Cost) {
	for _, f := range []struct {
		key string
		usd float64
	}{
		{"total", c.Total},
		{"input", c.Input},
		{"output", c.Output},
		{"cache_read", c.CacheRead},
		{"cache_write", c.CacheWrite},
		{"reasoning", c.Reasoning},
	} {
		if f.usd != 0 {
			o.span.SetAttributes(attribute.Float64(keyCostPrefix+f.key, f.usd))
		}
	}
}

// SetError marks the observation as failed with err's message.
func (o *Observation) SetError(err error) {
	if err != nil {
		o.span.SetStatus(codes.Error, err.Error())
	}
}

// End ends the observation. Its span is exported in the background.
func (o *Observation) End() { o.span.End() }

func encode(v any) string {
	switch x := v.(type) {
	case string:
		return x
	case []byte:
		return string(x)
	case json.RawMessage:
		return string(x)
	}
	b, err := json.Marshal(v)
	if err != nil {
		return fmt.Sprint(v)
	}
	return string(b)
}
