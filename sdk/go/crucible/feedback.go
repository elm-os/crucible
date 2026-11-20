package crucible

import (
	"fmt"
	"net/url"
	"regexp"
	"strings"

	"github.com/elm-os/crucible/sdk/go/internal/core"
)

// Feedback is a score on a trace (TraceID), a span (TraceID and SpanID) or a session (SessionID), written with
// source feedback (contracts 12.6).
type Feedback struct {
	// Name is the score name, for example "helpful", without '#'. A later score of the same name (and ScoreID)
	// replaces it.
	Name string
	// Value is a number (numeric), a bool (boolean) or a string (categorical, or text with DataType "text").
	Value any
	// TraceID is the trace, 32 hex characters.
	TraceID string
	// SpanID is a span of the trace, 16 hex characters.
	SpanID string
	// SessionID is a session, instead of a trace.
	SessionID string
	// ScoreID keeps scores of one name apart, for example one per reviewer.
	ScoreID string
	// DataType is how Crucible reads Value: "numeric", "boolean", "categorical" or "text". When empty, it follows
	// Value's type.
	DataType string
	// Comment is at most 4 KiB.
	Comment string
	// Metadata is a JSON object, at most 16 KiB.
	Metadata map[string]any
	// Retracted removes the score of this name (and ScoreID) from the target.
	Retracted bool
	// IdempotencyKey is sent as Idempotency-Key: 1 to 128 characters of A-Z, a-z, 0-9, '_' and '-'. Crucible writes a
	// score once per key and body for 24 hours. When empty, each call gets a random key.
	IdempotencyKey string
}

// SendFeedback queues one score for the project's feedback route (POST /api/v1/projects/{ProjectID}/feedback, with
// the secret key) and returns at once; it never blocks the caller and never panics on a network failure. One
// goroutine sends the scores in call order, each after the spans that ended before it, and retries a failed
// connection, a timeout, 408, 429, 5xx and a conflict 409 with the same Idempotency-Key, three attempts in all.
// Without Options.ProjectID, or when 1,024 scores are already waiting, the score is dropped. CurrentStats counts
// sent, dropped and failed scores. Before Init and after Shutdown it does nothing.
func SendFeedback(f Feedback) {
	c := core.Active()
	if c == nil {
		return
	}
	c.Feedback(core.FeedbackBody{
		TraceID:   f.TraceID,
		SpanID:    f.SpanID,
		SessionID: f.SessionID,
		Name:      f.Name,
		ScoreID:   f.ScoreID,
		Value:     f.Value,
		DataType:  f.DataType,
		Comment:   f.Comment,
		Metadata:  f.Metadata,
		Retracted: f.Retracted,
	}, f.IdempotencyKey)
}

var projectID = regexp.MustCompile(`^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$`)

// feedbackRoute is the project's feedback route, or "" without a project id.
func feedbackRoute(opts Options) (string, error) {
	base := opts.APIURL
	if base == "" {
		base = opts.Endpoint
	} else if u, err := url.Parse(base); err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" {
		return "", fmt.Errorf("crucible: Options.APIURL %q is not an http or https URL; set it to the API listener of your Crucible server, for example http://localhost:8080", base)
	}
	if opts.ProjectID == "" {
		return "", nil
	}
	if !projectID.MatchString(opts.ProjectID) {
		return "", fmt.Errorf("crucible: Options.ProjectID %q is not a project id; set it to the project's UUID, not its slug (crucible admin create-key prints it as project_id)", opts.ProjectID)
	}
	return strings.TrimRight(base, "/") + "/api/v1/projects/" + strings.ToLower(opts.ProjectID) + "/feedback", nil
}
