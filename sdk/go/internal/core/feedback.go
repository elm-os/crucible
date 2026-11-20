package core

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// The feedback sender (contract 12.6): a score with source feedback goes to POST
// {APIURL}/api/v1/projects/{ProjectID}/feedback with the project secret key, which needs the write scope (contract
// 11.3). Queueing never blocks, so the request leaves the caller's path as spans do (contract 9.10). One goroutine
// posts the queue in call order, one request at a time, because the latest score of a name wins. Before it posts,
// it flushes the span queue: the route answers 404 for a trace Crucible has not received, so the spans that ended
// before the call go first. A failed connection, a timeout, 408, 429, 5xx and a conflict 409 are retried, three
// attempts in all, after Retry-After (at most 10 s) or else 1 s and 2 s, with the same body and Idempotency-Key, so
// Crucible writes the score once. Flush returns why the last score that was not sent since the previous Flush was
// dropped or failed, with what to change.

const (
	// feedbackQueue bounds the scores waiting to be sent, the one being sent included.
	feedbackQueue    = 1024
	feedbackAttempts = 3
	feedbackTimeout  = 10 * time.Second
	retryAfterCap    = 10 * time.Second
)

// feedbackBackoff is the wait before the second and third attempts when the answer has no Retry-After.
var feedbackBackoff = [...]time.Duration{time.Second, 2 * time.Second}

const feedbackSettings = "scores go to Options.APIURL, the API listener, under Options.ProjectID, the key's project, with a secret key that has the write scope"

// FeedbackBody is the route's request body.
type FeedbackBody struct {
	TraceID   string         `json:"trace_id,omitempty"`
	SpanID    string         `json:"span_id,omitempty"`
	SessionID string         `json:"session_id,omitempty"`
	Name      string         `json:"name"`
	ScoreID   string         `json:"score_id,omitempty"`
	Value     any            `json:"value"`
	DataType  string         `json:"data_type,omitempty"`
	Comment   string         `json:"comment,omitempty"`
	Metadata  map[string]any `json:"metadata,omitempty"`
	Retracted bool           `json:"retracted,omitempty"`
}

type feedbackItem struct {
	seq  uint64
	body []byte
	key  string
}

// work is a score to send, or a flush marker whose reply is closed once everything queued before it is handled.
type work struct {
	item  *feedbackItem
	reply chan struct{}
}

// FeedbackSender posts scores in call order on one goroutine.
type FeedbackSender struct {
	url, secretKey string
	client         *http.Client
	flushSpans     func(context.Context) error

	mu     sync.Mutex
	seq    uint64
	closed bool
	// failure is why the last score that was not sent since the previous Flush was dropped or failed.
	failure string

	work     chan work
	stop     chan struct{}
	stopOnce sync.Once
	done     chan struct{}
	// ctx is cancelled when a shutdown runs out of time; it ends the request in flight and the waits between attempts.
	ctx    context.Context
	cancel context.CancelFunc

	pending               atomic.Int64
	sent, dropped, failed atomic.Uint64
}

// NewFeedbackSender starts a sender for the feedback route url; url is empty without a project id, and every score
// is then dropped and counted. flushSpans exports the spans queued so far.
func NewFeedbackSender(url, secretKey string, flushSpans func(context.Context) error) *FeedbackSender {
	ctx, cancel := context.WithCancel(context.Background())
	s := &FeedbackSender{
		url:        url,
		secretKey:  secretKey,
		client:     &http.Client{},
		flushSpans: flushSpans,
		work:       make(chan work, feedbackQueue),
		stop:       make(chan struct{}),
		done:       make(chan struct{}),
		ctx:        ctx,
		cancel:     cancel,
	}
	go s.run()
	return s
}

// Add queues one score with its Idempotency-Key (a random one when key is empty), or drops and counts it. It never
// blocks.
func (s *FeedbackSender) Add(body FeedbackBody, key string) {
	if s.url == "" {
		s.dropped.Add(1)
		s.fail("crucible: a score was dropped because Options.ProjectID is empty; set it to the key's project (crucible admin create-key prints it as project_id)")
		return
	}
	encoded, err := json.Marshal(body)
	if err != nil {
		s.dropped.Add(1)
		return
	}
	if key == "" {
		key = randomKey()
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed || s.pending.Load() >= feedbackQueue {
		s.dropped.Add(1)
		return
	}
	s.seq++
	select {
	case s.work <- work{item: &feedbackItem{seq: s.seq, body: encoded, key: key}}:
		s.pending.Add(1)
	default:
		s.dropped.Add(1)
	}
}

// Flush waits until every score queued before the call is handled, or ctx ends. It returns why the last score
// that was not sent since the previous Flush was dropped or failed.
func (s *FeedbackSender) Flush(ctx context.Context) error {
	reply := make(chan struct{})
	select {
	case s.work <- work{reply: reply}:
	case <-s.done:
		return s.takeFailure()
	case <-ctx.Done():
		return ctx.Err()
	}
	select {
	case <-reply:
	case <-s.done:
	case <-ctx.Done():
		return ctx.Err()
	}
	return s.takeFailure()
}

func (s *FeedbackSender) fail(reason string) {
	s.mu.Lock()
	s.failure = reason
	s.mu.Unlock()
}

func (s *FeedbackSender) takeFailure() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	reason := s.failure
	s.failure = ""
	if reason == "" {
		return nil
	}
	return errors.New(reason)
}

// Shutdown sends what is queued and stops. When ctx ends first, the request in flight is abandoned and the scores
// still queued are counted as dropped.
func (s *FeedbackSender) Shutdown(ctx context.Context) error {
	s.mu.Lock()
	s.closed = true
	s.mu.Unlock()
	s.stopOnce.Do(func() { close(s.stop) })
	select {
	case <-s.done:
		return nil
	case <-ctx.Done():
		s.cancel()
		return ctx.Err()
	}
}

// Counts returns the sent, dropped and failed scores.
func (s *FeedbackSender) Counts() (sent, dropped, failed uint64) {
	return s.sent.Load(), s.dropped.Load(), s.failed.Load()
}

func (s *FeedbackSender) run() {
	defer close(s.done)
	defer s.cancel()
	var flushed uint64
	for {
		var w work
		select {
		case w = <-s.work:
		case <-s.stop:
			select {
			case w = <-s.work:
			default:
				return
			}
		}
		if w.item == nil {
			close(w.reply)
			continue
		}
		switch {
		case s.ctx.Err() != nil:
			s.dropped.Add(1)
		default:
			if w.item.seq > flushed {
				s.mu.Lock()
				flushed = s.seq
				s.mu.Unlock()
				ctx, cancel := context.WithTimeout(s.ctx, feedbackTimeout)
				_ = s.flushSpans(ctx)
				cancel()
			}
			sent, reason := s.post(w.item)
			switch {
			case sent:
				s.sent.Add(1)
			case s.ctx.Err() != nil:
				s.dropped.Add(1)
			default:
				s.failed.Add(1)
				s.fail("crucible: a score was not sent: " + reason + "; " + feedbackSettings)
			}
		}
		s.pending.Add(-1)
	}
}

// post sends one score, retrying what may succeed later; it reports whether Crucible accepted it, and why not.
func (s *FeedbackSender) post(item *feedbackItem) (bool, string) {
	var wait time.Duration
	var reason string
	for attempt := range feedbackAttempts {
		if attempt > 0 {
			t := time.NewTimer(wait)
			select {
			case <-t.C:
			case <-s.ctx.Done():
				t.Stop()
				return false, reason
			}
		}
		sent, retry, after, why := s.request(item)
		if sent || !retry {
			return sent, why
		}
		reason = why
		wait = feedbackBackoff[min(attempt, len(feedbackBackoff)-1)]
		if after >= 0 {
			wait = after
		}
	}
	return false, reason
}

// request sends one attempt. after is the answer's Retry-After, or -1 without one; reason says why it was not sent.
func (s *FeedbackSender) request(item *feedbackItem) (sent, retry bool, after time.Duration, reason string) {
	ctx, cancel := context.WithTimeout(s.ctx, feedbackTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, s.url, bytes.NewReader(item.body))
	if err != nil {
		return false, false, -1, err.Error()
	}
	req.Header.Set("Authorization", "Bearer "+s.secretKey)
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Idempotency-Key", item.key)
	req.Header.Set("User-Agent", "crucible-sdk-go/"+Version)
	resp, err := s.client.Do(req)
	if err != nil {
		return false, true, -1, s.url + " could not be reached (" + err.Error() + ")"
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 64<<10))
	if resp.StatusCode >= 200 && resp.StatusCode < 300 {
		return true, false, 0, ""
	}
	reason = s.url + " answered " + strconv.Itoa(resp.StatusCode) + detail(body)
	if !retryable(resp.StatusCode, body) {
		return false, false, -1, reason
	}
	return false, true, retryAfter(resp.Header.Get("Retry-After")), reason
}

// problem is the part of a problem body that says what went wrong.
type problem struct {
	Code   string `json:"code"`
	Detail string `json:"detail"`
}

func detail(body []byte) string {
	var p problem
	if json.Unmarshal(body, &p) != nil || (p.Code == "" && p.Detail == "") {
		return ""
	}
	if p.Detail == "" {
		return " (" + p.Code + ")"
	}
	return " (" + p.Code + ": " + p.Detail + ")"
}

// retryable reports whether an answer may change on a later attempt: a timeout, throttling, a server fault or a key
// whose first request is still running (a conflict 409; idempotency_key_reused never changes).
func retryable(status int, body []byte) bool {
	switch {
	case status == http.StatusRequestTimeout, status == http.StatusTooManyRequests, status >= 500:
		return true
	case status != http.StatusConflict:
		return false
	}
	var p problem
	return json.Unmarshal(body, &p) == nil && p.Code == "conflict"
}

func retryAfter(header string) time.Duration {
	seconds, err := strconv.ParseFloat(strings.TrimSpace(header), 64)
	if err != nil || !(seconds >= 0) {
		return -1
	}
	if seconds >= retryAfterCap.Seconds() {
		return retryAfterCap
	}
	return time.Duration(seconds * float64(time.Second))
}

func randomKey() string {
	var b [16]byte
	_, _ = rand.Read(b[:])
	return hex.EncodeToString(b[:])
}
