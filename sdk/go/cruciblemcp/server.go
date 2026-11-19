package cruciblemcp

import (
	"context"
	"encoding/json"
	"math/rand/v2"
	"path"
	"strconv"
	"sync/atomic"

	"github.com/elm-os/crucible/sdk/go/internal/core"
	"github.com/elm-os/crucible/sdk/go/internal/scrub"
	"github.com/modelcontextprotocol/go-sdk/auth"
	"github.com/modelcontextprotocol/go-sdk/mcp"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/codes"
	"go.opentelemetry.io/otel/trace"
)

type server struct {
	serverName, serverVersion string
	transport                 Transport
	capture                   []CaptureRule
	handleParams              map[string]string
	identify                  func(context.Context, mcp.Request) Identity
	noIdentity                bool
	scrubFn                   func(field, value string) (string, bool)

	extras  extraTable
	headers headerTable
}

// Instrument adds receiving and sending middleware to s, so that every request and notification it receives
// becomes a SERVER span (contract 9.1). Add it after other middleware so that it sees the whole operation. Wrap
// the server's transport or HTTP handler with the returned Instrumentation so that the spans also carry the
// JSON-RPC request id. It returns an error, saying which option to change, when opts is invalid.
func Instrument(s *mcp.Server, opts Options) (*Instrumentation, error) {
	transport, err := checkTransport(opts.Transport, TransportStdio)
	if err != nil {
		return nil, err
	}
	capture, err := checkCapture(opts.Capture)
	if err != nil {
		return nil, err
	}
	in := &server{
		serverName:    opts.ServerName,
		serverVersion: opts.ServerVersion,
		transport:     transport,
		capture:       capture,
		handleParams:  opts.HandleParams,
		identify:      opts.Identify,
		noIdentity:    opts.NoIdentity,
		scrubFn:       opts.Scrub,
	}
	s.AddReceivingMiddleware(in.receive)
	s.AddSendingMiddleware(in.send)
	return &Instrumentation{s: in}, nil
}

// common returns the keys every server span carries.
func (s *server) common(method string) []attribute.KeyValue {
	kv := []attribute.KeyValue{
		attribute.String(keyMethod, method),
		attribute.String(keyCaptureSource, "sdk"),
	}
	if s.serverName != "" {
		kv = append(kv, attribute.String(keyServerName, s.serverName))
	}
	if s.serverVersion != "" {
		kv = append(kv, attribute.String(keyServerVersion, s.serverVersion))
	}
	return kv
}

// requestInfo is implemented by every *mcp.ServerRequest: it reads the per-request _meta of the modern era and
// falls back to the session's initialize parameters in the legacy era.
type requestInfo interface {
	ProtocolVersion() string
	ClientInfo() *mcp.Implementation
}

func (s *server) receive(next mcp.MethodHandler) mcp.MethodHandler {
	return func(ctx context.Context, method string, req mcp.Request) (mcp.Result, error) {
		params := req.GetParams()
		id := s.requestID(req.GetExtra())
		o := &op{}
		ctx = context.WithValue(extract(ctx, params), opKey{}, o)
		ctx, span := core.Tracer().Start(ctx, spanName(method, params), trace.WithSpanKind(trace.SpanKindServer))
		defer span.End()
		if !span.IsRecording() {
			return next(ctx, method, req)
		}

		kv := s.common(method)
		kv = append(kv, targetAttr(method, params)...)
		if id != "" {
			kv = append(kv, attribute.String(keyRequestID, id))
		}
		notification := isNotification(method)
		if notification {
			kv = append(kv, attribute.Bool(keyIsNotification, true))
		}

		var version string
		var client *mcp.Implementation
		if ri, ok := req.(requestInfo); ok {
			version, client = ri.ProtocolVersion(), ri.ClientInfo()
		}
		if ip, ok := params.(*mcp.InitializeParams); ok && ip != nil {
			version, client = ip.ProtocolVersion, ip.ClientInfo
		}
		if client != nil {
			if client.Name != "" {
				kv = append(kv, attribute.String(keyClientName, client.Name))
			}
			if client.Version != "" {
				kv = append(kv, attribute.String(keyClientVersion, client.Version))
			}
		}

		transport := s.transport
		extra := req.GetExtra()
		if extra != nil && extra.Header != nil {
			transport = TransportStreamableHTTP
			if ua := extra.Header.Get("User-Agent"); ua != "" {
				kv = append(kv, attribute.String(keyUserAgent, ua))
			}
			if n, err := strconv.ParseInt(extra.Header.Get("Content-Length"), 10, 64); err == nil && n >= 0 {
				kv = append(kv, attribute.Int64(keyRequestBytes, n))
			}
		}
		kv = append(kv, attribute.String(keyTransport, string(transport)))

		handle := ""
		if !s.noIdentity {
			var idkv []attribute.KeyValue
			idkv, handle = s.identity(ctx, req, extra)
			kv = append(kv, idkv...)
		}

		tool := ""
		var args json.RawMessage
		if p, ok := params.(*mcp.CallToolParamsRaw); ok && p != nil {
			tool, args = p.Name, p.Arguments
		}
		if handle == "" && tool != "" {
			handle = s.handleParam(tool, args)
		}
		if handle != "" {
			kv = append(kv, attribute.String(keyHandleID, handle))
		}
		if state := requestState(params); state != "" {
			if h := core.StateHash(state); h != "" {
				kv = append(kv, attribute.String(keyStateIn, h))
			}
		}

		rule := s.rule(tool)
		truncated := false
		if rule != nil && rule.Args {
			window, cut := scrub.Window(args, rule.MaxBytes)
			noteWork(len(window), len(window))
			truncated = cut
			if text, cut, ok := s.captured(FieldArguments, window, rule.MaxBytes); ok {
				kv = append(kv, attribute.String(keyToolArguments, text))
				truncated = truncated || cut
			}
		}
		span.SetAttributes(kv...)

		res, err := next(ctx, method, req)

		if r, ok := res.(*mcp.InitializeResult); ok && r != nil && r.ProtocolVersion != "" {
			version = r.ProtocolVersion
		}
		kv = eraAttrs(version)
		if version != "" && version < modernVersion {
			if id := req.GetSession().ID(); id != "" {
				kv = append(kv, attribute.String(keySessionID, core.SessionKey(id)))
			}
		}
		out := observe(ctx, res, err, true)
		// In the modern era over HTTP, closing the response stream is the cancellation signal.
		if out.cancel != "" && transport == TransportStreamableHTTP && version >= modernVersion {
			out.cancel = "stream_close"
		}
		okv, class := outcomeAttrs(out, o, notification, true)
		kv = append(kv, okv...)
		if r, ok := res.(*mcp.CallToolResult); ok && r != nil && rule != nil && rule.Result {
			if text, cut, ok := s.capturedResult(r, rule.MaxBytes); ok {
				kv = append(kv, attribute.String(keyToolResult, text))
				truncated = truncated || cut
			}
		}
		if rule != nil {
			kv = append(kv, attribute.Bool(keyContentCaptured, true), attribute.Float64(keyContentSampleRate, rule.SampleRate))
			truncation := "none"
			if truncated {
				truncation = "cap_exceeded"
			}
			kv = append(kv, attribute.String(keyContentTruncation, truncation))
		}
		span.SetAttributes(kv...)
		if class != "none" {
			msg, _, _ := s.captured(FieldErrorMessage, out.message, maxErrorMessage)
			span.SetStatus(codes.Error, msg)
		}
		return res, err
	}
}

// send counts the progress notifications a handler sends for its request.
func (s *server) send(next mcp.MethodHandler) mcp.MethodHandler {
	return func(ctx context.Context, method string, req mcp.Request) (mcp.Result, error) {
		if method == "notifications/progress" {
			if o := opFrom(ctx); o != nil {
				o.progress.Add(1)
			}
		}
		return next(ctx, method, req)
	}
}

// identity returns the identity keys of contract 9.3 and the handle id the Identify hook gave.
func (s *server) identity(ctx context.Context, req mcp.Request, extra *mcp.RequestExtra) ([]attribute.KeyValue, string) {
	var id Identity
	if s.identify != nil {
		id = s.identify(ctx, req)
	}
	subject, issuer, clientID := id.Subject, id.Issuer, ""
	var token *auth.TokenInfo
	if extra != nil {
		token = extra.TokenInfo
	}
	if token != nil {
		if subject == "" {
			subject, issuer = token.UserID, claim(token, "iss")
		}
		clientID = claim(token, "client_id")
	}
	source := ""
	switch {
	case id != Identity{}:
		source = "identify_hook"
	case token != nil && token.UserID != "":
		source = "token_verified"
	}
	var kv []attribute.KeyValue
	add := func(k, v string) {
		if v != "" {
			kv = append(kv, attribute.String(k, v))
		}
	}
	add(keyUserID, id.UserID)
	add(keyTenant, id.Tenant)
	add(keyUserSubject, subject)
	add(keyUserIssuer, issuer)
	add(keyOAuthClientID, clientID)
	add(keyIdentitySource, source)
	handle, _ := scrub.Prefix(id.HandleID, maxHandleID)
	return kv, handle
}

func claim(t *auth.TokenInfo, name string) string {
	v, _ := t.Extra[name].(string)
	return v
}

// handleParam returns the scrubbed string value of the tool's handle argument (contract 9.8), or "".
func (s *server) handleParam(tool string, args json.RawMessage) string {
	name := s.handleParams[tool]
	if name == "" {
		return ""
	}
	var top map[string]json.RawMessage
	if json.Unmarshal(args, &top) != nil {
		return ""
	}
	var v string
	if json.Unmarshal(top[name], &v) != nil {
		return ""
	}
	h, _, _ := s.captured(FieldHandleID, v, maxHandleID)
	return h
}

// rule returns the capture rule of a sampled tool call, or nil when nothing is captured. The sampling decision
// comes before anything is copied.
func (s *server) rule(tool string) *CaptureRule {
	if tool == "" {
		return nil
	}
	for i := range s.capture {
		r := &s.capture[i]
		if ok, _ := path.Match(r.Tool, tool); ok {
			if r.SampleRate < 1 && rand.Float64() >= r.SampleRate {
				return nil
			}
			return r
		}
	}
	return nil
}

// workHook, when set, is called once per captured field with the bytes of JSON the capture path serialized for
// it before any cut and the bytes it handed to the scrubber. The capture-overhead scenario reads its work bound
// from it.
var workHook atomic.Pointer[func(serialized, scrubbed int)]

func noteWork(serialized, scrubbed int) {
	if h := workHook.Load(); h != nil {
		(*h)(serialized, scrubbed)
	}
}

// captured scrubs and caps one value, then applies the Scrub option. ok is false when there is nothing to
// send.
func (s *server) captured(field, value string, max int) (text string, truncated, ok bool) {
	text, truncated = scrub.Capped(value, max)
	if s.scrubFn != nil {
		if text, ok = s.scrubFn(field, text); !ok {
			return "", truncated, false
		}
		text, _ = scrub.Prefix(text, max)
	}
	return text, truncated, text != ""
}

// capturedResult is the captured form of a tool result, scrubbed and capped as captured does it.
func (s *server) capturedResult(r *mcp.CallToolResult, max int) (text string, truncated, ok bool) {
	result := resultText(r)
	noteWork(len(result), min(len(result), scrub.WindowSize(max)))
	return s.captured(FieldResult, result, max)
}

// resultText is the captured form of a tool result: its content and structured content only, never the
// request state or input requests of a multi-round exchange.
func resultText(r *mcp.CallToolResult) string {
	b, err := json.Marshal(struct {
		Content           []mcp.Content `json:"content"`
		StructuredContent any           `json:"structuredContent,omitempty"`
		IsError           bool          `json:"isError,omitempty"`
	}{r.Content, r.StructuredContent, r.IsError})
	if err != nil {
		return ""
	}
	return string(b)
}
