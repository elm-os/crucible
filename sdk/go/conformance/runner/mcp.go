package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/elm-os/crucible/sdk/go/crucible"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

// exchange is one message sent to the reference server and, for a request, its response.
type exchange struct {
	step          string
	id            string // compact JSON of the request id; empty for a notification
	done          chan struct{}
	message       json.RawMessage
	httpStatus    *int
	sessionID     *string
	notifications int
	cancel        context.CancelFunc
	err           error
}

// peer sends messages to the reference server over one transport.
type peer interface {
	send(ex *exchange, msg []byte, headers map[string]string)
	stop() error
}

type event struct {
	Event         string          `json:"event"`
	Step          string          `json:"step"`
	HTTPStatus    *int            `json:"http_status"`
	SessionID     *string         `json:"session_id"`
	Message       json.RawMessage `json:"message"`
	Notifications int             `json:"notifications"`
}

func runMCP(spec runSpec) error {
	var p peer
	var err error
	switch spec.Transport {
	case "stdio":
		p, err = startStdio(spec)
	case "streamable_http":
		p, err = startHTTP(spec)
	default:
		err = fmt.Errorf("unknown transport %q", spec.Transport)
	}
	if err != nil {
		return err
	}
	responses := map[string]json.RawMessage{}
	pending := map[string]*exchange{}
	out := json.NewEncoder(os.Stdout)
	finish := func(ex *exchange) error {
		<-ex.done
		if ex.err != nil {
			return fmt.Errorf("step %s: %w", ex.step, ex.err)
		}
		responses[ex.step] = ex.message
		return out.Encode(event{
			Event: "response", Step: ex.step, HTTPStatus: ex.httpStatus, SessionID: ex.sessionID,
			Message: ex.message, Notifications: ex.notifications,
		})
	}
	for _, st := range spec.Steps {
		switch {
		case st.Send != nil:
			msg, id, err := resolve(st.Send, responses)
			if err != nil {
				return fmt.Errorf("step %s: %w", st.Step, err)
			}
			ex := &exchange{step: st.Step, id: id, done: make(chan struct{})}
			p.send(ex, msg, st.Headers)
			if st.Wait != nil && !*st.Wait {
				pending[st.Step] = ex
				continue
			}
			if err := finish(ex); err != nil {
				return err
			}
		case st.Await != "":
			ex, ok := pending[st.Await]
			if !ok {
				return fmt.Errorf("step %s awaits %s, which is not in flight", st.Step, st.Await)
			}
			delete(pending, st.Await)
			if err := finish(ex); err != nil {
				return err
			}
		case st.SleepMS != nil:
			time.Sleep(time.Duration(*st.SleepMS) * time.Millisecond)
		case st.Close != "":
			ex, ok := pending[st.Close]
			if !ok || ex.cancel == nil {
				return fmt.Errorf("step %s closes %s, which is not an HTTP exchange in flight", st.Step, st.Close)
			}
			delete(pending, st.Close)
			ex.cancel()
		default:
			return fmt.Errorf("step %s: not an MCP step", st.Step)
		}
	}
	for _, ex := range pending {
		if ex.cancel != nil {
			ex.cancel()
		}
	}
	return p.stop()
}

// resolve replaces every {"$from": "<step>/<pointer>"} in a message with the value the RFC 6901 pointer selects
// in that step's response, and returns the compact message and the compact JSON of its id.
func resolve(raw json.RawMessage, responses map[string]json.RawMessage) ([]byte, string, error) {
	var msg map[string]json.RawMessage
	if err := json.Unmarshal(raw, &msg); err != nil {
		return nil, "", err
	}
	id := ""
	if v, ok := msg["id"]; ok {
		id = compact(v)
	}
	if !bytes.Contains(raw, []byte(`"$from"`)) {
		return []byte(compact(raw)), id, nil
	}
	d := json.NewDecoder(bytes.NewReader(raw))
	d.UseNumber()
	var v any
	if err := d.Decode(&v); err != nil {
		return nil, "", err
	}
	v, err := substitute(v, responses)
	if err != nil {
		return nil, "", err
	}
	b, err := json.Marshal(v)
	return b, id, err
}

func substitute(v any, responses map[string]json.RawMessage) (any, error) {
	switch x := v.(type) {
	case map[string]any:
		if from, ok := x["$from"].(string); ok && len(x) == 1 {
			return pointer(from, responses)
		}
		for k, e := range x {
			r, err := substitute(e, responses)
			if err != nil {
				return nil, err
			}
			x[k] = r
		}
	case []any:
		for i, e := range x {
			r, err := substitute(e, responses)
			if err != nil {
				return nil, err
			}
			x[i] = r
		}
	}
	return v, nil
}

// pointer selects "<step>/<pointer>" in a step's response.
func pointer(from string, responses map[string]json.RawMessage) (any, error) {
	stepName, path, _ := strings.Cut(from, "/")
	raw, ok := responses[stepName]
	if !ok {
		return nil, fmt.Errorf("$from %q names a step with no response", from)
	}
	d := json.NewDecoder(bytes.NewReader(raw))
	d.UseNumber()
	var v any
	if err := d.Decode(&v); err != nil {
		return nil, err
	}
	for _, tok := range strings.Split(path, "/") {
		tok = strings.ReplaceAll(strings.ReplaceAll(tok, "~1", "/"), "~0", "~")
		switch x := v.(type) {
		case map[string]any:
			if v, ok = x[tok]; !ok {
				return nil, fmt.Errorf("$from %q: no member %q", from, tok)
			}
		case []any:
			i, err := strconv.Atoi(tok)
			if err != nil || i < 0 || i >= len(x) {
				return nil, fmt.Errorf("$from %q: no element %q", from, tok)
			}
			v = x[i]
		default:
			return nil, fmt.Errorf("$from %q: cannot step into %q", from, tok)
		}
	}
	return v, nil
}

// stdioPeer runs the reference server as a child process and speaks JSON lines over its stdin and stdout.
type stdioPeer struct {
	cmd      *exec.Cmd
	stdin    io.WriteCloser
	mu       sync.Mutex
	inflight []*exchange
	exited   chan struct{}
}

func startStdio(spec runSpec) (*stdioPeer, error) {
	self, err := os.Executable()
	if err != nil {
		return nil, err
	}
	spec.Steps = nil
	arg, err := json.Marshal(spec)
	if err != nil {
		return nil, err
	}
	cmd := exec.Command(self, "serve-stdio", string(arg))
	cmd.Stderr = os.Stderr
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return nil, err
	}
	if err := cmd.Start(); err != nil {
		return nil, err
	}
	p := &stdioPeer{cmd: cmd, stdin: stdin, exited: make(chan struct{})}
	go p.read(stdout)
	return p, nil
}

func (p *stdioPeer) send(ex *exchange, msg []byte, _ map[string]string) {
	if ex.id != "" {
		p.mu.Lock()
		p.inflight = append(p.inflight, ex)
		p.mu.Unlock()
	}
	if _, err := p.stdin.Write(append(msg, '\n')); err != nil {
		ex.err = err
		close(ex.done)
		return
	}
	if ex.id == "" {
		close(ex.done)
	}
}

// read matches each response line to its request; every other line is a server message for the request most
// recently sent.
func (p *stdioPeer) read(stdout io.Reader) {
	defer close(p.exited)
	sc := bufio.NewScanner(stdout)
	sc.Buffer(nil, 64<<20)
	for sc.Scan() {
		line := append([]byte(nil), sc.Bytes()...)
		var m struct {
			ID     json.RawMessage `json:"id"`
			Result json.RawMessage `json:"result"`
			Error  json.RawMessage `json:"error"`
		}
		if json.Unmarshal(line, &m) != nil {
			continue
		}
		p.mu.Lock()
		if m.ID != nil && (m.Result != nil || m.Error != nil) {
			id := compact(m.ID)
			for i, ex := range p.inflight {
				if ex.id == id {
					p.inflight = append(p.inflight[:i], p.inflight[i+1:]...)
					ex.message = line
					close(ex.done)
					break
				}
			}
		} else if n := len(p.inflight); n > 0 {
			p.inflight[n-1].notifications++
		}
		p.mu.Unlock()
	}
}

func (p *stdioPeer) stop() error {
	_ = p.stdin.Close()
	<-p.exited
	return p.cmd.Wait()
}

// httpPeer serves the reference server in process on 127.0.0.1 and sends each message as its own POST.
type httpPeer struct {
	url     string
	server  *http.Server
	client  *http.Client
	mu      sync.Mutex
	session string
}

func startHTTP(spec runSpec) (*httpPeer, error) {
	if err := initSDK(spec); err != nil {
		return nil, err
	}
	s, in, err := newReferenceServer(spec)
	if err != nil {
		return nil, err
	}
	getServer := func(*http.Request) *mcp.Server { return s }
	// The reference server is dual era: an initialize request opens a legacy session, and a request at the
	// modern version is served statelessly, with a closed stream cancelling it.
	legacy := mcp.NewStreamableHTTPHandler(getServer, nil)
	modern := mcp.NewStreamableHTTPHandler(getServer, &mcp.StreamableHTTPOptions{Stateless: true, PropagateRequestCancellation: true})
	route := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Mcp-Protocol-Version") >= "2026-07-28" {
			modern.ServeHTTP(w, r)
			return
		}
		legacy.ServeHTTP(w, r)
	})
	mux := http.NewServeMux()
	mux.Handle("/mcp", in.Handler(route))
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return nil, err
	}
	p := &httpPeer{
		url:    "http://" + ln.Addr().String() + "/mcp",
		server: &http.Server{Handler: mux, ReadHeaderTimeout: 10 * time.Second},
		client: &http.Client{},
	}
	go func() { _ = p.server.Serve(ln) }()
	return p, nil
}

func (p *httpPeer) send(ex *exchange, msg []byte, headers map[string]string) {
	ctx, cancel := context.WithCancel(context.Background())
	ex.cancel = cancel
	go func() {
		defer close(ex.done)
		req, err := http.NewRequestWithContext(ctx, http.MethodPost, p.url, bytes.NewReader(msg))
		if err != nil {
			ex.err = err
			return
		}
		req.Header.Set("Content-Type", "application/json")
		req.Header.Set("Accept", "application/json, text/event-stream")
		for k, v := range headers {
			req.Header.Set(k, v)
		}
		p.mu.Lock()
		if p.session != "" {
			req.Header.Set("Mcp-Session-Id", p.session)
		}
		p.mu.Unlock()
		resp, err := p.client.Do(req)
		if err != nil {
			ex.err = err
			return
		}
		defer resp.Body.Close()
		status := resp.StatusCode
		ex.httpStatus = &status
		if sid := resp.Header.Get("Mcp-Session-Id"); sid != "" {
			ex.sessionID = &sid
			if ex.step == "initialize" {
				p.mu.Lock()
				p.session = sid
				p.mu.Unlock()
			}
		}
		if ex.id == "" {
			_, _ = io.Copy(io.Discard, resp.Body)
			return
		}
		if !strings.HasPrefix(resp.Header.Get("Content-Type"), "text/event-stream") {
			ex.message, ex.err = io.ReadAll(resp.Body)
			return
		}
		ex.err = readEvents(resp.Body, ex)
	}()
}

// readEvents reads an event stream until the response with the exchange's id; the messages before it count as
// server messages for the request.
func readEvents(body io.Reader, ex *exchange) error {
	sc := bufio.NewScanner(body)
	sc.Buffer(nil, 64<<20)
	var data bytes.Buffer
	// dispatch takes the buffered event; it reports whether the event was the response.
	dispatch := func() bool {
		msg := append([]byte(nil), data.Bytes()...)
		data.Reset()
		var m struct {
			ID     json.RawMessage `json:"id"`
			Result json.RawMessage `json:"result"`
			Error  json.RawMessage `json:"error"`
		}
		if json.Unmarshal(msg, &m) == nil && m.ID != nil && compact(m.ID) == ex.id && (m.Result != nil || m.Error != nil) {
			ex.message = msg
			return true
		}
		ex.notifications++
		return false
	}
	for sc.Scan() {
		line := sc.Text()
		if v, ok := strings.CutPrefix(line, "data:"); ok {
			if data.Len() > 0 {
				data.WriteByte('\n')
			}
			data.WriteString(strings.TrimPrefix(v, " "))
			continue
		}
		if line == "" && data.Len() > 0 && dispatch() {
			return nil
		}
	}
	if err := sc.Err(); err != nil {
		return err
	}
	if data.Len() > 0 && dispatch() {
		return nil
	}
	return errors.New("the event stream ended before the response")
}

// stop shuts the server down, which ends the open exchanges, and then flushes the SDK. The client's idle
// connections close first: the server would otherwise wait out a connection the client dialled and never used.
func (p *httpPeer) stop() error {
	p.client.CloseIdleConnections()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	_ = p.server.Shutdown(ctx)
	sctx, scancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer scancel()
	return crucible.Shutdown(sctx)
}
