package crucible_test

import (
	"context"
	"encoding/json"
	"reflect"
	"regexp"
	"strings"
	"testing"
	"time"

	"github.com/elm-os/crucible/sdk/go/crucible"
	"github.com/elm-os/crucible/sdk/go/internal/collectortest"
)

const (
	testProject = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b"
	testTrace   = "4bf92f3577b34da6a3ce929d0e0e4736"
	feedbackURL = "/api/v1/projects/" + testProject + "/feedback"
)

// startFeedback starts the SDK with a project id and a flush interval long enough that spans leave only through
// Flush, Shutdown or the feedback sender.
func startFeedback(t *testing.T, c *collectortest.Collector) {
	t.Helper()
	start(t, c, crucible.Options{ProjectID: testProject, FlushInterval: time.Hour})
}

func flushed(t *testing.T) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if err := crucible.Flush(ctx); err != nil {
		t.Fatalf("Flush: %v", err)
	}
}

func eventually(t *testing.T, what string, cond func() bool) {
	t.Helper()
	deadline := time.Now().Add(15 * time.Second)
	for !cond() {
		if time.Now().After(deadline) {
			t.Fatalf("%s did not happen in time", what)
		}
		time.Sleep(5 * time.Millisecond)
	}
}

func decoded(t *testing.T, r collectortest.Request) map[string]any {
	t.Helper()
	var body map[string]any
	if err := json.Unmarshal(r.Body, &body); err != nil {
		t.Fatalf("feedback body %q: %v", r.Body, err)
	}
	return body
}

func TestFeedbackPostsOneScoreToTheProjectRouteWithTheSecretKey(t *testing.T) {
	c := collectortest.New(t)
	startFeedback(t, c)
	crucible.SendFeedback(crucible.Feedback{
		TraceID: testTrace, Name: "helpful", Value: 1, Comment: "good answer", IdempotencyKey: "thumbs-4bf9",
	})
	flushed(t)
	posts := c.Feedback()
	if len(posts) != 1 {
		t.Fatalf("%d feedback posts, want 1", len(posts))
	}
	r := posts[0]
	if r.Method != "POST" || r.Path != feedbackURL {
		t.Errorf("request %s %s, want POST %s", r.Method, r.Path, feedbackURL)
	}
	if r.Authorization != "Bearer "+testKey || r.ContentType != "application/json" || r.IdempotencyKey != "thumbs-4bf9" {
		t.Errorf("headers: Authorization %q, Content-Type %q, Idempotency-Key %q", r.Authorization, r.ContentType, r.IdempotencyKey)
	}
	want := map[string]any{"trace_id": testTrace, "name": "helpful", "value": 1.0, "comment": "good answer"}
	if got := decoded(t, r); !reflect.DeepEqual(got, want) {
		t.Errorf("body %v, want %v", got, want)
	}
	if s := crucible.CurrentStats(); s.FeedbackSent != 1 || s.FeedbackDropped != 0 || s.FeedbackFailed != 0 {
		t.Errorf("stats %+v", s)
	}
}

func TestFeedbackSendsEveryFieldUnderTheRouteNameAndAKeyPerCall(t *testing.T) {
	c := collectortest.New(t)
	startFeedback(t, c)
	crucible.SendFeedback(crucible.Feedback{
		TraceID: testTrace, SpanID: "00f067aa0ba902b7", Name: "tone", Value: "friendly", ScoreID: "reviewer-2",
		DataType: "categorical", Metadata: map[string]any{"source": "thumbs", "rating": 4}, Retracted: true,
	})
	crucible.SendFeedback(crucible.Feedback{SessionID: "conversation-17", Name: "resolved", Value: true})
	flushed(t)
	posts := c.Feedback()
	if len(posts) != 2 {
		t.Fatalf("%d feedback posts, want 2", len(posts))
	}
	span := map[string]any{
		"trace_id": testTrace, "span_id": "00f067aa0ba902b7", "name": "tone", "value": "friendly",
		"score_id": "reviewer-2", "data_type": "categorical",
		"metadata": map[string]any{"source": "thumbs", "rating": 4.0}, "retracted": true,
	}
	if got := decoded(t, posts[0]); !reflect.DeepEqual(got, span) {
		t.Errorf("span body %v, want %v", got, span)
	}
	session := map[string]any{"session_id": "conversation-17", "name": "resolved", "value": true}
	if got := decoded(t, posts[1]); !reflect.DeepEqual(got, session) {
		t.Errorf("session body %v, want %v", got, session)
	}
	key := regexp.MustCompile(`^[0-9a-f]{32}$`)
	if !key.MatchString(posts[0].IdempotencyKey) || !key.MatchString(posts[1].IdempotencyKey) ||
		posts[0].IdempotencyKey == posts[1].IdempotencyKey {
		t.Errorf("keys %q and %q, want two different random keys", posts[0].IdempotencyKey, posts[1].IdempotencyKey)
	}
}

func TestFeedbackFollowsTheSpansThatEndedBeforeIt(t *testing.T) {
	c := collectortest.New(t)
	startFeedback(t, c)
	_, o := crucible.Observe(context.Background(), "answer", crucible.Agent)
	o.End()
	crucible.SendFeedback(crucible.Feedback{TraceID: testTrace, Name: "helpful", Value: 1})
	eventually(t, "a feedback post", func() bool { return len(c.Feedback()) == 1 })
	var paths []string
	for _, r := range c.Requests() {
		paths = append(paths, r.Path)
	}
	if len(paths) < 2 || paths[0] != "/v1/traces" || paths[len(paths)-1] != feedbackURL {
		t.Errorf("requests %v, want the span export before the feedback post", paths)
	}
}

func TestFeedbackRetriesA503WithTheSameKeyAndBody(t *testing.T) {
	c := collectortest.New(t)
	startFeedback(t, c)
	c.AnswerFeedback(collectortest.FeedbackAnswer{Status: 503, Header: map[string]string{"Retry-After": "0"}})
	crucible.SendFeedback(crucible.Feedback{TraceID: testTrace, Name: "helpful", Value: 0.5})
	flushed(t)
	posts := c.Feedback()
	if len(posts) != 2 {
		t.Fatalf("%d feedback posts, want 2", len(posts))
	}
	if posts[0].IdempotencyKey != posts[1].IdempotencyKey || string(posts[0].Body) != string(posts[1].Body) {
		t.Errorf("the retry changed: %q %s, then %q %s", posts[0].IdempotencyKey, posts[0].Body, posts[1].IdempotencyKey, posts[1].Body)
	}
	if s := crucible.CurrentStats(); s.FeedbackSent != 1 || s.FeedbackFailed != 0 {
		t.Errorf("stats %+v", s)
	}
}

func TestFeedbackRetriesAConflictButNotAReusedKey(t *testing.T) {
	c := collectortest.New(t)
	startFeedback(t, c)
	c.AnswerFeedback(
		collectortest.FeedbackAnswer{Status: 409, Header: map[string]string{"Retry-After": "0"}, Body: `{"code":"conflict","status":409}`},
		collectortest.FeedbackAnswer{Status: 201},
		collectortest.FeedbackAnswer{Status: 409, Body: `{"code":"idempotency_key_reused","status":409}`},
	)
	crucible.SendFeedback(crucible.Feedback{TraceID: testTrace, Name: "first", Value: 1})
	crucible.SendFeedback(crucible.Feedback{TraceID: testTrace, Name: "second", Value: 1})
	flushed(t)
	var names []string
	for _, r := range c.Feedback() {
		names = append(names, decoded(t, r)["name"].(string))
	}
	if strings.Join(names, ",") != "first,first,second" {
		t.Errorf("posts %v, want first, first, second", names)
	}
	if s := crucible.CurrentStats(); s.FeedbackSent != 1 || s.FeedbackFailed != 1 {
		t.Errorf("stats %+v", s)
	}
}

func TestFeedbackCountsARefusalWithoutRetrying(t *testing.T) {
	c := collectortest.New(t)
	startFeedback(t, c)
	c.AnswerFeedback(collectortest.FeedbackAnswer{Status: 422, Body: `{"code":"validation_failed"}`})
	crucible.SendFeedback(crucible.Feedback{TraceID: testTrace, Name: "bad#name", Value: 1})
	flushed(t)
	if n := len(c.Feedback()); n != 1 {
		t.Errorf("%d feedback posts, want 1", n)
	}
	if s := crucible.CurrentStats(); s.FeedbackFailed != 1 || s.FeedbackSent != 0 {
		t.Errorf("stats %+v", s)
	}
}

func TestFeedbackCountsAFailingConnectionAfterThreeAttempts(t *testing.T) {
	c := collectortest.New(t)
	startFeedback(t, c)
	c.AnswerFeedback(collectortest.FeedbackAnswer{}, collectortest.FeedbackAnswer{}, collectortest.FeedbackAnswer{})
	crucible.SendFeedback(crucible.Feedback{TraceID: testTrace, Name: "helpful", Value: 1})
	eventually(t, "a failed score", func() bool { return crucible.CurrentStats().FeedbackFailed == 1 })
	if n := len(c.Feedback()); n != 3 {
		t.Errorf("%d feedback posts, want 3", n)
	}
}

func TestFeedbackReturnsAtOnceWhileTheAPIStalls(t *testing.T) {
	c := collectortest.NewStalled(t)
	if err := crucible.Init(crucible.Options{Endpoint: c.URL, SecretKey: testKey, ProjectID: testProject, FlushInterval: time.Hour}); err != nil {
		t.Fatal(err)
	}
	began := time.Now()
	for i := range 100 {
		crucible.SendFeedback(crucible.Feedback{TraceID: testTrace, Name: "stalled", Value: i})
	}
	if d := time.Since(began); d > 100*time.Millisecond {
		t.Errorf("100 calls took %v", d)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 200*time.Millisecond)
	defer cancel()
	began = time.Now()
	_ = crucible.Shutdown(ctx)
	if d := time.Since(began); d > 2*time.Second {
		t.Errorf("Shutdown took %v past its deadline", d)
	}
}

func TestFeedbackDropsAndCountsWhenTheQueueIsFull(t *testing.T) {
	c := collectortest.NewStalled(t)
	if err := crucible.Init(crucible.Options{Endpoint: c.URL, SecretKey: testKey, ProjectID: testProject, FlushInterval: time.Hour}); err != nil {
		t.Fatal(err)
	}
	defer func() {
		ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
		defer cancel()
		_ = crucible.Shutdown(ctx)
	}()
	for i := range 1100 {
		crucible.SendFeedback(crucible.Feedback{TraceID: testTrace, Name: "burst", Value: i})
	}
	if s := crucible.CurrentStats(); s.FeedbackDropped != 76 {
		t.Errorf("dropped %d, want 76", s.FeedbackDropped)
	}
}

func TestFeedbackWithoutAProjectIDIsDroppedAndCounted(t *testing.T) {
	c := collectortest.New(t)
	start(t, c, crucible.Options{})
	crucible.SendFeedback(crucible.Feedback{TraceID: testTrace, Name: "helpful", Value: 1})
	flushed(t)
	if n := len(c.Feedback()); n != 0 {
		t.Errorf("%d feedback posts, want 0", n)
	}
	if s := crucible.CurrentStats(); s.FeedbackDropped != 1 {
		t.Errorf("stats %+v", s)
	}
}

func TestFeedbackGoesToAPIURLWhenSet(t *testing.T) {
	api := collectortest.New(t)
	if err := crucible.Init(crucible.Options{
		Endpoint: "http://127.0.0.1:9", APIURL: api.URL + "/", SecretKey: testKey, ProjectID: strings.ToUpper(testProject),
	}); err != nil {
		t.Fatal(err)
	}
	defer func() {
		ctx, cancel := context.WithTimeout(context.Background(), time.Second)
		defer cancel()
		_ = crucible.Shutdown(ctx)
	}()
	crucible.SendFeedback(crucible.Feedback{TraceID: testTrace, Name: "helpful", Value: 1})
	eventually(t, "a feedback post", func() bool { return len(api.Feedback()) == 1 })
	if p := api.Feedback()[0].Path; p != feedbackURL {
		t.Errorf("path %s, want %s", p, feedbackURL)
	}
}

func TestShutdownSendsQueuedFeedbackAndLaterFeedbackIsNotSent(t *testing.T) {
	c := collectortest.New(t)
	if err := crucible.Init(crucible.Options{Endpoint: c.URL, SecretKey: testKey, ProjectID: testProject}); err != nil {
		t.Fatal(err)
	}
	crucible.SendFeedback(crucible.Feedback{TraceID: testTrace, Name: "last-words", Value: 1})
	if err := crucible.Shutdown(context.Background()); err != nil {
		t.Fatal(err)
	}
	crucible.SendFeedback(crucible.Feedback{TraceID: testTrace, Name: "too-late", Value: 1})
	time.Sleep(50 * time.Millisecond)
	posts := c.Feedback()
	if len(posts) != 1 || decoded(t, posts[0])["name"] != "last-words" {
		t.Errorf("posts %v, want only last-words", posts)
	}
}

func TestInitRefusesBadFeedbackSettings(t *testing.T) {
	const ok = "http://localhost:4318"
	for _, tc := range []struct {
		opts crucible.Options
		want string
	}{
		{crucible.Options{Endpoint: ok, SecretKey: testKey, ProjectID: "default"}, "Options.ProjectID \"default\" is not a project id"},
		{crucible.Options{Endpoint: ok, SecretKey: testKey, APIURL: "ftp://host"}, "Options.APIURL \"ftp://host\" is not an http or https URL"},
	} {
		err := crucible.Init(tc.opts)
		if err == nil {
			_ = crucible.Shutdown(context.Background())
			t.Fatalf("Init(%+v) succeeded", tc.opts)
		}
		if !strings.Contains(err.Error(), tc.want) {
			t.Errorf("Init(%+v) = %q, want it to say %q", tc.opts, err, tc.want)
		}
	}
}
