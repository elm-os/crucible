package cruciblemcp

import (
	"context"

	"github.com/elm-os/crucible/sdk/go/internal/core"
	"github.com/elm-os/crucible/sdk/go/internal/scrub"
	"github.com/modelcontextprotocol/go-sdk/mcp"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/codes"
	"go.opentelemetry.io/otel/trace"
)

// ClientOptions configures InstrumentClient.
type ClientOptions struct {
	// Transport is the transport the client connects over. Empty means unknown.
	Transport Transport
}

type client struct {
	transport Transport
}

// InstrumentClient adds sending middleware to c, so that every request and notification it sends becomes a
// CLIENT span whose trace context travels to the server in params._meta (traceparent, tracestate). A server
// instrumented with Instrument then records its span as a child of the client's. It returns an error, saying
// which option to change, when opts is invalid.
func InstrumentClient(c *mcp.Client, opts ClientOptions) error {
	transport, err := checkTransport(opts.Transport, TransportUnknown)
	if err != nil {
		return err
	}
	in := &client{transport: transport}
	c.AddSendingMiddleware(in.send)
	return nil
}

func (c *client) send(next mcp.MethodHandler) mcp.MethodHandler {
	return func(ctx context.Context, method string, req mcp.Request) (mcp.Result, error) {
		params := req.GetParams()
		ctx, span := core.Tracer().Start(ctx, spanName(method, params), trace.WithSpanKind(trace.SpanKindClient))
		defer span.End()
		inject(ctx, params)
		if !span.IsRecording() {
			return next(ctx, method, req)
		}

		session, _ := req.GetSession().(*mcp.ClientSession)
		var version string
		var info *mcp.Implementation
		if session != nil {
			if r := session.InitializeResult(); r != nil {
				version, info = r.ProtocolVersion, r.ServerInfo
			}
		}
		if v, ok := meta(params)[mcp.MetaKeyProtocolVersion].(string); ok && v != "" {
			version = v
		}
		if ip, ok := params.(*mcp.InitializeParams); ok && ip != nil {
			version = ip.ProtocolVersion
		}

		notification := isNotification(method)
		kv := []attribute.KeyValue{
			attribute.String(keyMethod, method),
			attribute.String(keyCaptureSource, "sdk"),
			attribute.String(keyTransport, string(c.transport)),
		}
		kv = append(kv, targetAttr(method, params)...)
		if notification {
			kv = append(kv, attribute.Bool(keyIsNotification, true))
		}
		span.SetAttributes(kv...)

		res, err := next(ctx, method, req)

		switch r := res.(type) {
		case *mcp.InitializeResult:
			if r != nil {
				version, info = r.ProtocolVersion, r.ServerInfo
			}
		case *mcp.DiscoverResult:
			if r != nil {
				info = serverInfo(r.GetMeta())
			}
		}
		kv = eraAttrs(version)
		if info != nil && info.Name != "" {
			kv = append(kv, attribute.String(keyServerName, info.Name))
			if info.Version != "" {
				kv = append(kv, attribute.String(keyServerVersion, info.Version))
			}
		}
		if version != "" && version < modernVersion && session != nil {
			if id := session.ID(); id != "" {
				kv = append(kv, attribute.String(keySessionID, core.SessionKey(id)))
			}
		}
		out := observe(ctx, res, err, false)
		// This middleware sees neither the server's annotations nor the progress notifications the client
		// receives, so the count stays absent: unobserved.
		okv, class := outcomeAttrs(out, &op{}, notification, false)
		kv = append(kv, okv...)
		span.SetAttributes(kv...)
		if class != "none" {
			msg, _ := scrub.Capped(out.message, maxErrorMessage)
			span.SetStatus(codes.Error, msg)
		}
		return res, err
	}
}

// serverInfo reads the server's identity from a result's _meta (the modern era sends it on every result).
func serverInfo(m map[string]any) *mcp.Implementation {
	switch v := m[mcp.MetaKeyServerInfo].(type) {
	case *mcp.Implementation:
		return v
	case map[string]any:
		name, _ := v["name"].(string)
		version, _ := v["version"].(string)
		return &mcp.Implementation{Name: name, Version: version}
	}
	return nil
}
