package cruciblemcp

import (
	"context"
	"errors"
	"reflect"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"

	"github.com/elm-os/crucible/sdk/go/internal/core"
	"github.com/modelcontextprotocol/go-sdk/jsonrpc"
	"github.com/modelcontextprotocol/go-sdk/mcp"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/propagation"
	"go.opentelemetry.io/otel/trace"
)

// Subclass refines a tool execution error (crucible.mcp.error.subclass).
type Subclass string

// The subclasses.
const (
	SubclassAuth      Subclass = "auth"
	SubclassRateLimit Subclass = "rate_limit"
	SubclassUpstream  Subclass = "upstream"
	SubclassBusiness  Subclass = "business"
)

// op is the state of one operation that its handler can annotate through the context.
type op struct {
	validation, notFound atomic.Bool
	progress             atomic.Uint32
	mu                   sync.Mutex
	subclass             Subclass
}

type opKey struct{}

func opFrom(ctx context.Context) *op {
	o, _ := ctx.Value(opKey{}).(*op)
	return o
}

// AnnotateError records why a tool failed. It refines a tool execution error, the class of a result with
// isError: true. Call it from the tool handler with the handler's context.
func AnnotateError(ctx context.Context, s Subclass) {
	if o := opFrom(ctx); o != nil {
		o.mu.Lock()
		o.subclass = s
		o.mu.Unlock()
	}
}

// MarkValidation records that the tool rejected its arguments: an isError result counts as a validation
// error instead of a tool execution error.
func MarkValidation(ctx context.Context) {
	if o := opFrom(ctx); o != nil {
		o.validation.Store(true)
	}
}

// MarkNotFound records that what the request named does not exist: an isError result, or a JSON-RPC
// invalid-params error (-32602), counts as not found.
func MarkNotFound(ctx context.Context) {
	if o := opFrom(ctx); o != nil {
		o.notFound.Store(true)
	}
}

// propagator reads and writes only the W3C trace context; baggage is never sent (contract 9.1).
var propagator = propagation.TraceContext{}

func isNil(p mcp.Params) bool {
	if p == nil {
		return true
	}
	v := reflect.ValueOf(p)
	return v.Kind() == reflect.Pointer && v.IsNil()
}

func meta(p mcp.Params) map[string]any {
	if isNil(p) {
		return nil
	}
	return p.GetMeta()
}

func spanName(method string, p mcp.Params) string {
	if t := target(p); t != "" {
		return method + " " + t
	}
	return method
}

// target is the tool or prompt a request names. Resource URIs are never used: the template a URI matched is
// not visible here, and a raw URI is no rollup dimension.
func target(p mcp.Params) string {
	switch p := p.(type) {
	case *mcp.CallToolParamsRaw:
		if p != nil {
			return p.Name
		}
	case *mcp.CallToolParams:
		if p != nil {
			return p.Name
		}
	case *mcp.GetPromptParams:
		if p != nil {
			return p.Name
		}
	}
	return ""
}

func targetAttr(method string, p mcp.Params) []attribute.KeyValue {
	t := target(p)
	switch {
	case t == "":
		return nil
	case method == "tools/call":
		return []attribute.KeyValue{attribute.String(keyToolName, t)}
	case method == "prompts/get":
		return []attribute.KeyValue{attribute.String(keyPromptName, t)}
	}
	return nil
}

func requestState(p mcp.Params) string {
	switch p := p.(type) {
	case *mcp.CallToolParamsRaw:
		if p != nil {
			return p.RequestState
		}
	case *mcp.CallToolParams:
		if p != nil {
			return p.RequestState
		}
	case *mcp.GetPromptParams:
		if p != nil {
			return p.RequestState
		}
	case *mcp.ReadResourceParams:
		if p != nil {
			return p.RequestState
		}
	}
	return ""
}

func eraAttrs(version string) []attribute.KeyValue {
	if version == "" {
		return nil
	}
	era := "legacy"
	if version >= modernVersion {
		era = "modern"
	}
	return []attribute.KeyValue{attribute.String(keyProtocolVersion, version), attribute.String(keyEra, era)}
}

// outcome is what the middleware saw of one operation's end.
type outcome struct {
	cancel     string // "", "notification" or "stream_close"
	timedOut   bool
	hasCode    bool
	code       int64
	isError    bool
	needsInput bool
	state      string
	message    string
	blocks     []mcp.Content
}

func observe(ctx context.Context, res mcp.Result, err error, server bool) outcome {
	var out outcome
	switch ctxErr := ctx.Err(); {
	case errors.Is(ctxErr, context.Canceled):
		out.cancel = "notification"
		// On a server, a notifications/cancelled message cancels the request with no cause; a broken
		// connection cancels it with the read error as the cause.
		if server && context.Cause(ctx) != context.Canceled {
			out.cancel = "stream_close"
		}
	case errors.Is(ctxErr, context.DeadlineExceeded), errors.Is(err, context.DeadlineExceeded):
		out.timedOut = true
	}
	if err != nil {
		out.hasCode = true
		var wire *jsonrpc.Error
		if errors.As(err, &wire) {
			out.code = wire.Code
		}
		out.message = err.Error()
	}
	switch r := res.(type) {
	case *mcp.CallToolResult:
		if r == nil {
			break
		}
		out.isError = r.IsError
		out.needsInput = r.NeedsInput()
		out.state = r.RequestState
		out.blocks = r.Content
		if r.IsError {
			out.message = toolErrorText(r)
		}
		// mcp.AddTool turns a handler's error into an isError result; a deadline the handler hit is a
		// server-side timeout all the same.
		if out.cancel == "" && errors.Is(r.GetError(), context.DeadlineExceeded) {
			out.timedOut = true
		}
	case *mcp.GetPromptResult:
		if r != nil {
			out.needsInput, out.state = r.NeedsInput(), r.RequestState
		}
	case *mcp.ReadResourceResult:
		if r != nil {
			out.needsInput, out.state = r.NeedsInput(), r.RequestState
		}
	}
	return out
}

func toolErrorText(r *mcp.CallToolResult) string {
	if err := r.GetError(); err != nil {
		return err.Error()
	}
	for _, c := range r.Content {
		if t, ok := c.(*mcp.TextContent); ok {
			return t.Text
		}
	}
	return ""
}

// errorClass is the first matching row of the classifier table (contract 9.2) for what the middleware can
// observe. HTTP statuses and transport failures happen outside the SDK's middleware and are never seen here.
func errorClass(out outcome, o *op) string {
	switch {
	case out.cancel != "":
		return "cancelled"
	case out.timedOut:
		return "timeout"
	case out.hasCode:
		switch out.code {
		case -32700, -32600, -32601, -32020, -32021, -32022:
			return "protocol"
		case -32002:
			return "not_found"
		case -32602:
			if o.notFound.Load() {
				return "not_found"
			}
			return "validation"
		}
		return "internal"
	case out.isError:
		if o.validation.Load() {
			return "validation"
		}
		if o.notFound.Load() {
			return "not_found"
		}
		return "tool_execution"
	}
	return "none"
}

// outcomeAttrs returns the outcome keys of contract 9.1 and the error class; message is the scrubbed error
// message, for the span status.
func outcomeAttrs(out outcome, o *op, notification bool) (kv []attribute.KeyValue, class string) {
	class = errorClass(out, o)
	if class != "none" {
		kv = append(kv, attribute.String(keyErrorClass, class))
	}
	if class == "tool_execution" {
		o.mu.Lock()
		if o.subclass != "" {
			kv = append(kv, attribute.String(keyErrorSubclass, string(o.subclass)))
		}
		o.mu.Unlock()
	}
	switch {
	case out.hasCode:
		code := strconv.FormatInt(out.code, 10)
		kv = append(kv, attribute.String(keyErrorType, code), attribute.String(keyRPCStatusCode, code))
	case out.isError:
		kv = append(kv, attribute.String(keyErrorType, "tool_error"))
	}
	if out.cancel != "" {
		kv = append(kv, attribute.String(keyCancelSource, out.cancel))
	}
	if out.timedOut {
		kv = append(kv, attribute.Bool(keyTimedOut, true))
	}
	if notification {
		return kv, class
	}
	// A result type exists for a request that returned a result and was neither cancelled nor timed out.
	if !out.hasCode && out.cancel == "" && !out.timedOut {
		resultType := "complete"
		if out.needsInput {
			resultType = "input_required"
		}
		kv = append(kv, attribute.String(keyResultType, resultType))
	}
	if out.needsInput && out.state != "" {
		if h := core.StateHash(out.state); h != "" {
			kv = append(kv, attribute.String(keyStateOut, h))
		}
	}
	kv = append(kv, attribute.Int64(keyProgressCount, int64(o.progress.Load())))
	kv = append(kv, blockAttrs(out.blocks)...)
	return kv, class
}

func blockAttrs(content []mcp.Content) []attribute.KeyValue {
	if len(content) == 0 {
		return nil
	}
	counts := map[string]int64{}
	for _, c := range content {
		counts[blockType(c)]++
	}
	keys := make([]string, 0, len(counts))
	for k := range counts {
		keys = append(keys, k)
	}
	slices.Sort(keys)
	kv := make([]attribute.KeyValue, 0, len(keys))
	for _, k := range keys {
		kv = append(kv, attribute.Int64(keyContentBlocks+k, counts[k]))
	}
	return kv
}

func blockType(c mcp.Content) string {
	switch c.(type) {
	case *mcp.TextContent:
		return "text"
	case *mcp.ImageContent:
		return "image"
	case *mcp.AudioContent:
		return "audio"
	case *mcp.ResourceLink:
		return "resource_link"
	case *mcp.EmbeddedResource:
		return "resource"
	}
	return "other"
}

func isNotification(method string) bool { return strings.HasPrefix(method, "notifications/") }

// inject writes the span's trace context into the request's _meta, on a copy of the map.
func inject(ctx context.Context, p mcp.Params) {
	if isNil(p) || !trace.SpanContextFromContext(ctx).IsValid() {
		return
	}
	carrier := propagation.MapCarrier{}
	propagator.Inject(ctx, carrier)
	m := make(map[string]any, len(p.GetMeta())+len(carrier))
	for k, v := range p.GetMeta() {
		m[k] = v
	}
	for k, v := range carrier {
		m[k] = v
	}
	p.SetMeta(m)
}

// extract returns ctx with the remote span context that the request's _meta carries, if any.
func extract(ctx context.Context, p mcp.Params) context.Context {
	m := meta(p)
	carrier := propagation.MapCarrier{}
	for _, k := range propagator.Fields() {
		if v, ok := m[k].(string); ok {
			carrier[k] = v
		}
	}
	if len(carrier) == 0 {
		return ctx
	}
	return propagator.Extract(ctx, carrier)
}
