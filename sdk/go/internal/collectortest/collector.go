// Package collectortest is an OTLP/HTTP trace collector for tests. It keeps every request it receives, raw
// and decoded. It also answers the feedback route, POST /api/v1/projects/{id}/feedback: with the scripted answers
// while any are left, else with 201.
package collectortest

import (
	"encoding/hex"
	"io"
	"net/http"
	"net/http/httptest"
	"regexp"
	"sync"
	"testing"

	coltracepb "go.opentelemetry.io/proto/otlp/collector/trace/v1"
	commonpb "go.opentelemetry.io/proto/otlp/common/v1"
	tracepb "go.opentelemetry.io/proto/otlp/trace/v1"
	"google.golang.org/protobuf/proto"
)

// Request is one request as received.
type Request struct {
	Method, Path, ContentType, Authorization, UserAgent, IdempotencyKey string
	Body                                                                []byte
}

// FeedbackAnswer is one scripted answer to a feedback post. Status 0 closes the connection without answering.
type FeedbackAnswer struct {
	Status int
	Header map[string]string
	Body   string
}

var feedbackPath = regexp.MustCompile(`^/api/v1/projects/[^/]+/feedback$`)

// Span is one received span with its attributes decoded to Go values: string, bool, int64, float64,
// []string or []any.
type Span struct {
	Name          string
	Kind          tracepb.Span_SpanKind
	TraceID       string
	SpanID        string
	ParentSpanID  string
	StatusCode    tracepb.Status_StatusCode
	StatusMessage string
	Scope         string
	Attrs         map[string]any
	Resource      map[string]any
}

// Collector is an httptest server that accepts OTLP/HTTP protobuf exports on /v1/traces.
type Collector struct {
	*httptest.Server
	mu       sync.Mutex
	requests []Request
	spans    []Span
	answers  []FeedbackAnswer
	stall    chan struct{}
}

// New starts a collector that answers every export with success. It closes when the test ends.
func New(t testing.TB) *Collector {
	c := &Collector{}
	c.Server = httptest.NewServer(http.HandlerFunc(c.handle))
	t.Cleanup(c.Close)
	return c
}

// NewStalled starts a collector that accepts connections and never answers, until the test ends.
func NewStalled(t testing.TB) *Collector {
	c := &Collector{stall: make(chan struct{})}
	c.Server = httptest.NewServer(http.HandlerFunc(c.handle))
	t.Cleanup(func() {
		close(c.stall)
		c.Close()
	})
	return c
}

func (c *Collector) handle(w http.ResponseWriter, r *http.Request) {
	if c.stall != nil {
		select {
		case <-c.stall:
		case <-r.Context().Done():
		}
		return
	}
	body, err := io.ReadAll(r.Body)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}
	req := Request{
		Method:         r.Method,
		Path:           r.URL.Path,
		ContentType:    r.Header.Get("Content-Type"),
		Authorization:  r.Header.Get("Authorization"),
		UserAgent:      r.Header.Get("User-Agent"),
		IdempotencyKey: r.Header.Get("Idempotency-Key"),
		Body:           body,
	}
	if r.Method == http.MethodPost && feedbackPath.MatchString(r.URL.Path) {
		c.answerFeedback(w, req)
		return
	}
	var export coltracepb.ExportTraceServiceRequest
	if err := proto.Unmarshal(body, &export); err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}
	c.mu.Lock()
	c.requests = append(c.requests, req)
	for _, rs := range export.GetResourceSpans() {
		res := decodeAttrs(rs.GetResource().GetAttributes())
		for _, ss := range rs.GetScopeSpans() {
			for _, s := range ss.GetSpans() {
				c.spans = append(c.spans, Span{
					Name:          s.GetName(),
					Kind:          s.GetKind(),
					TraceID:       hex.EncodeToString(s.GetTraceId()),
					SpanID:        hex.EncodeToString(s.GetSpanId()),
					ParentSpanID:  hex.EncodeToString(s.GetParentSpanId()),
					StatusCode:    s.GetStatus().GetCode(),
					StatusMessage: s.GetStatus().GetMessage(),
					Scope:         ss.GetScope().GetName(),
					Attrs:         decodeAttrs(s.GetAttributes()),
					Resource:      res,
				})
			}
		}
	}
	c.mu.Unlock()
	out, _ := proto.Marshal(&coltracepb.ExportTraceServiceResponse{})
	w.Header().Set("Content-Type", "application/x-protobuf")
	_, _ = w.Write(out)
}

func (c *Collector) answerFeedback(w http.ResponseWriter, req Request) {
	c.mu.Lock()
	c.requests = append(c.requests, req)
	answer := FeedbackAnswer{Status: http.StatusCreated, Header: map[string]string{"Content-Type": "application/json"}, Body: `{"key":"k"}`}
	if len(c.answers) > 0 {
		answer, c.answers = c.answers[0], c.answers[1:]
	}
	c.mu.Unlock()
	if answer.Status == 0 {
		if conn, _, err := http.NewResponseController(w).Hijack(); err == nil {
			_ = conn.Close()
		}
		return
	}
	for k, v := range answer.Header {
		w.Header().Set(k, v)
	}
	w.WriteHeader(answer.Status)
	_, _ = io.WriteString(w, answer.Body)
}

// AnswerFeedback scripts the answers to the next feedback posts, in order.
func (c *Collector) AnswerFeedback(answers ...FeedbackAnswer) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.answers = append(c.answers, answers...)
}

// Feedback returns the feedback posts received so far, in arrival order.
func (c *Collector) Feedback() []Request {
	var out []Request
	for _, r := range c.Requests() {
		if feedbackPath.MatchString(r.Path) {
			out = append(out, r)
		}
	}
	return out
}

// Requests returns the requests received so far, export and feedback, in arrival order.
func (c *Collector) Requests() []Request {
	c.mu.Lock()
	defer c.mu.Unlock()
	return append([]Request(nil), c.requests...)
}

// Spans returns the spans received so far, in arrival order.
func (c *Collector) Spans() []Span {
	c.mu.Lock()
	defer c.mu.Unlock()
	return append([]Span(nil), c.spans...)
}

// Bodies returns every request body received so far, concatenated.
func (c *Collector) Bodies() []byte {
	var all []byte
	for _, r := range c.Requests() {
		all = append(all, r.Body...)
	}
	return all
}

func decodeAttrs(kvs []*commonpb.KeyValue) map[string]any {
	m := make(map[string]any, len(kvs))
	for _, kv := range kvs {
		m[kv.GetKey()] = decodeValue(kv.GetValue())
	}
	return m
}

func decodeValue(v *commonpb.AnyValue) any {
	switch x := v.GetValue().(type) {
	case *commonpb.AnyValue_StringValue:
		return x.StringValue
	case *commonpb.AnyValue_BoolValue:
		return x.BoolValue
	case *commonpb.AnyValue_IntValue:
		return x.IntValue
	case *commonpb.AnyValue_DoubleValue:
		return x.DoubleValue
	case *commonpb.AnyValue_ArrayValue:
		values := x.ArrayValue.GetValues()
		strs := make([]string, 0, len(values))
		for _, e := range values {
			s, ok := e.GetValue().(*commonpb.AnyValue_StringValue)
			if !ok {
				anys := make([]any, 0, len(values))
				for _, e := range values {
					anys = append(anys, decodeValue(e))
				}
				return anys
			}
			strs = append(strs, s.StringValue)
		}
		return strs
	case *commonpb.AnyValue_BytesValue:
		return x.BytesValue
	default:
		return nil
	}
}
