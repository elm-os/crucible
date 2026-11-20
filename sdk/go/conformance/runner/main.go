// Command runner plays the shared SDK conformance suite against this SDK (docs/testing/sdk-conformance.md,
// runner contract version 1). crucible-sdk-driver starts it once per case and writes the run spec to its stdin.
// For an agent case the runner is the agent program; for an MCP case it starts the reference server, as a child
// process for stdio and in process for streamable HTTP, and plays the steps against it. "runner serve-stdio
// <spec>" is the stdio reference server.
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"time"

	"github.com/elm-os/crucible/sdk/go/crucible"
)

type runSpec struct {
	Case            string `json:"case"`
	Kind            string `json:"kind"`
	Era             string `json:"era"`
	Transport       string `json:"transport"`
	ProtocolVersion string `json:"protocol_version"`
	OTLP            struct {
		Endpoint string `json:"endpoint"`
	} `json:"otlp"`
	CheckpointURL string `json:"checkpoint_url"`
	APIURL        string `json:"api_url"`
	ProjectID     string `json:"project_id"`
	Keys          struct {
		SecretKey string `json:"secret_key"`
	} `json:"keys"`
	SDK    sdkOptions `json:"sdk"`
	Server struct {
		Name    string `json:"name"`
		Version string `json:"version"`
	} `json:"server"`
	Steps []step `json:"steps"`
}

type sdkOptions struct {
	ServiceName     string            `json:"service_name"`
	DeployID        string            `json:"deploy_id"`
	Environment     string            `json:"environment"`
	Release         string            `json:"release"`
	FlushIntervalMS int64             `json:"flush_interval_ms"`
	Capture         []captureRule     `json:"capture"`
	HandleParams    map[string]string `json:"handle_params"`
}

type captureRule struct {
	Tool       string  `json:"tool"`
	Args       bool    `json:"args"`
	Result     bool    `json:"result"`
	SampleRate float64 `json:"sample_rate"`
	MaxBytes   int     `json:"max_bytes"`
}

type step struct {
	Step       string            `json:"step"`
	Send       json.RawMessage   `json:"send"`
	Headers    map[string]string `json:"headers"`
	Wait       *bool             `json:"wait"`
	Await      string            `json:"await"`
	SleepMS    *int64            `json:"sleep_ms"`
	Close      string            `json:"close"`
	Observe    *node             `json:"observe"`
	Flush      json.RawMessage   `json:"flush"`
	Checkpoint string            `json:"checkpoint"`
	Shutdown   json.RawMessage   `json:"shutdown"`
	Feedback   *feedbackStep     `json:"feedback"`
}

// feedbackStep is one score: the route's body fields and the caller's Idempotency-Key.
type feedbackStep struct {
	TraceID        string         `json:"trace_id"`
	SpanID         string         `json:"span_id"`
	SessionID      string         `json:"session_id"`
	Name           string         `json:"name"`
	ScoreID        string         `json:"score_id"`
	Value          any            `json:"value"`
	DataType       string         `json:"data_type"`
	Comment        string         `json:"comment"`
	Metadata       map[string]any `json:"metadata"`
	Retracted      bool           `json:"retracted"`
	IdempotencyKey string         `json:"idempotency_key"`
}

func main() {
	var err error
	if len(os.Args) == 3 && os.Args[1] == "serve-stdio" {
		err = serveStdio(os.Args[2])
	} else {
		err = run()
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "runner:", err)
		os.Exit(1)
	}
}

func run() error {
	var spec runSpec
	if err := json.NewDecoder(os.Stdin).Decode(&spec); err != nil {
		return fmt.Errorf("reading the run spec: %w", err)
	}
	switch spec.Kind {
	case "agent":
		return runAgent(spec)
	case "mcp":
		return runMCP(spec)
	}
	return fmt.Errorf("case %s: unknown kind %q", spec.Case, spec.Kind)
}

// initSDK starts the SDK with the run spec's options.
func initSDK(spec runSpec) error {
	return crucible.Init(crucible.Options{
		Endpoint:      spec.OTLP.Endpoint,
		APIURL:        spec.APIURL,
		ProjectID:     spec.ProjectID,
		SecretKey:     spec.Keys.SecretKey,
		ServiceName:   spec.SDK.ServiceName,
		Environment:   spec.SDK.Environment,
		Release:       spec.SDK.Release,
		DeployID:      spec.SDK.DeployID,
		FlushInterval: time.Duration(spec.SDK.FlushIntervalMS) * time.Millisecond,
	})
}
