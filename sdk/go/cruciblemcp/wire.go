package cruciblemcp

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"net/http"
	"reflect"
	"runtime"
	"strconv"
	"sync"
	"weak"

	"github.com/modelcontextprotocol/go-sdk/jsonrpc"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

// Instrumentation is one instrumented server. The MCP Go SDK does not pass middleware the JSON-RPC id of a
// request; Transport and Handler wrap what the server is connected over so that its spans carry it.
type Instrumentation struct {
	s *server
}

// Transport wraps a transport the server connects over (stdio, an io.ReadCloser and io.WriteCloser pair, in
// memory, a command), so that each request's span carries its JSON-RPC id (jsonrpc.request.id). The wrapped
// connection does not reject JSON-RPC batches in protocol versions that removed them, because the MCP SDK tells
// only its own connection type the negotiated version.
func (in *Instrumentation) Transport(t mcp.Transport) mcp.Transport {
	return &transport{inner: t, ids: &in.s.extras}
}

// maxPeek is how much of a POST body Handler reads to find the JSON-RPC id: the MCP SDK's default body limit.
const maxPeek = mcp.DefaultMaxRequestBodyBytes

// Handler wraps a streamable HTTP handler from mcp.NewStreamableHTTPHandler, so that each request's span carries
// its JSON-RPC id. It reads each POST body, up to 4 MiB, and passes it on unchanged. Wrap the MCP handler
// directly: a handler in between must not replace the request's header map.
func (in *Instrumentation) Handler(h http.Handler) http.Handler {
	ids := &in.s.headers
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost || r.Body == nil {
			h.ServeHTTP(w, r)
			return
		}
		prefix, err := io.ReadAll(io.LimitReader(r.Body, maxPeek))
		r.Body = replayBody{Reader: io.MultiReader(bytes.NewReader(prefix), r.Body), Closer: r.Body}
		if err == nil && len(prefix) < maxPeek {
			if id := peekID(prefix); id != "" {
				key := headerKey(r.Header)
				ids.put(key, id)
				defer ids.drop(key)
			}
		}
		h.ServeHTTP(w, r)
	})
}

type replayBody struct {
	io.Reader
	io.Closer
}

// peekID returns the JSON-RPC id of one message in the string form of jsonrpc.request.id, or "".
func peekID(body []byte) string {
	var m struct {
		ID json.RawMessage `json:"id"`
	}
	if json.Unmarshal(body, &m) != nil || len(m.ID) == 0 {
		return ""
	}
	var s string
	if json.Unmarshal(m.ID, &s) == nil {
		return s
	}
	var f float64
	if json.Unmarshal(m.ID, &f) == nil {
		// The MCP SDK reads a numeric id as an int64.
		return strconv.FormatInt(int64(f), 10)
	}
	return ""
}

func headerKey(h http.Header) uintptr {
	if h == nil {
		return 0
	}
	return reflect.ValueOf(h).Pointer()
}

// idString is the string form of a JSON-RPC id (jsonrpc.request.id).
func idString(id jsonrpc.ID) string {
	switch v := id.Raw().(type) {
	case int64:
		return strconv.FormatInt(v, 10)
	case string:
		return v
	}
	return ""
}

type transport struct {
	inner mcp.Transport
	ids   *extraTable
}

func (t *transport) Connect(ctx context.Context) (mcp.Connection, error) {
	c, err := t.inner.Connect(ctx)
	if err != nil {
		return nil, err
	}
	return &conn{Connection: c, ids: t.ids}, nil
}

// SupportsProtocolVersion forwards to the wrapped transport, which supports every version when it does not say.
func (t *transport) SupportsProtocolVersion(version string) bool {
	if p, ok := t.inner.(mcp.ProtocolVersionSupporter); ok {
		return p.SupportsProtocolVersion(version)
	}
	return true
}

type conn struct {
	mcp.Connection
	ids *extraTable
}

// Read gives every call a RequestExtra of its own, through which the middleware finds the call's id.
func (c *conn) Read(ctx context.Context) (jsonrpc.Message, error) {
	msg, err := c.Connection.Read(ctx)
	if r, ok := msg.(*jsonrpc.Request); ok && r.IsCall() && r.Extra == nil {
		x := &mcp.RequestExtra{}
		r.Extra = x
		c.ids.put(x, idString(r.ID))
	}
	return msg, err
}

// extraTable maps the RequestExtra that Transport gave a call to the call's id. Its keys are weak, and an entry
// goes when the middleware takes it or when its RequestExtra is collected, so a call that never reaches the
// middleware leaves nothing behind.
type extraTable struct {
	mu sync.Mutex
	m  map[weak.Pointer[mcp.RequestExtra]]string
}

func (t *extraTable) put(x *mcp.RequestExtra, id string) {
	k := weak.Make(x)
	t.mu.Lock()
	if t.m == nil {
		t.m = make(map[weak.Pointer[mcp.RequestExtra]]string)
	}
	t.m[k] = id
	t.mu.Unlock()
	runtime.AddCleanup(x, t.drop, k)
}

func (t *extraTable) take(x *mcp.RequestExtra) (string, bool) {
	k := weak.Make(x)
	t.mu.Lock()
	defer t.mu.Unlock()
	id, ok := t.m[k]
	delete(t.m, k)
	return id, ok
}

func (t *extraTable) drop(k weak.Pointer[mcp.RequestExtra]) {
	t.mu.Lock()
	delete(t.m, k)
	t.mu.Unlock()
}

// headerTable maps the header map of a POST that Handler is serving to the JSON-RPC id in its body. The MCP SDK
// hands that same map to the middleware as RequestExtra.Header.
type headerTable struct {
	mu sync.Mutex
	m  map[uintptr]string
}

func (t *headerTable) put(k uintptr, id string) {
	if k == 0 {
		return
	}
	t.mu.Lock()
	if t.m == nil {
		t.m = make(map[uintptr]string)
	}
	t.m[k] = id
	t.mu.Unlock()
}

func (t *headerTable) get(k uintptr) string {
	t.mu.Lock()
	defer t.mu.Unlock()
	return t.m[k]
}

func (t *headerTable) drop(k uintptr) {
	t.mu.Lock()
	delete(t.m, k)
	t.mu.Unlock()
}

// requestID returns the JSON-RPC id of the request that carries x, when Transport or Handler saw it.
func (s *server) requestID(x *mcp.RequestExtra) string {
	if x == nil {
		return ""
	}
	if id, ok := s.extras.take(x); ok {
		return id
	}
	if x.Header != nil {
		return s.headers.get(headerKey(x.Header))
	}
	return ""
}
