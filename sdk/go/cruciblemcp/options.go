// Package cruciblemcp instruments servers and clients of the official MCP Go SDK, in both protocol eras. Each
// operation becomes one span that carries the MCP wire attributes Crucible reads (contract 9.1); call
// crucible.Init first so the spans are exported.
package cruciblemcp

import (
	"context"
	"fmt"
	"path"

	"github.com/modelcontextprotocol/go-sdk/mcp"
)

// Transport names how requests reach a server (crucible.mcp.transport).
type Transport string

// The transports.
const (
	TransportUnknown        Transport = "unknown"
	TransportStdio          Transport = "stdio"
	TransportStreamableHTTP Transport = "streamable_http"
	TransportSSE            Transport = "sse"
)

// CaptureRule opts the arguments and results of matching tools into capture. Content is off by default.
type CaptureRule struct {
	// Tool is a glob over tool names in path.Match syntax; "*" matches every tool.
	Tool string
	// Args and Result say what to capture.
	Args, Result bool
	// SampleRate is the share of matching calls captured, decided before anything is copied. 0 means 1.
	SampleRate float64
	// MaxBytes caps each captured field, cut on a character boundary. 0 means 16 KiB.
	MaxBytes int
}

// Identity is what an Identify hook knows about the caller of one request. Every field is optional.
type Identity struct {
	// UserID is your application's user id (user.id).
	UserID string
	// Issuer and Subject identify a token's user. Crucible hashes the subject with a per-project salt and
	// keeps only the hash and the issuer.
	Issuer, Subject string
	// Tenant names the caller's tenant.
	Tenant string
	// HandleID groups the operations of one logical session (contract 9.8).
	HandleID string
}

// The fields a Scrub function is called for.
const (
	FieldArguments    = "arguments"
	FieldResult       = "result"
	FieldErrorMessage = "error_message"
	FieldHandleID     = "handle_id"
)

// Options configures Instrument.
type Options struct {
	// ServerName and ServerVersion name the server in Crucible. When empty, Crucible uses the service name
	// and version given to crucible.Init.
	ServerName, ServerVersion string
	// Transport is reported for requests that did not arrive over streamable HTTP, which is detected. Empty
	// means stdio.
	Transport Transport
	// Capture opts tools into argument and result capture. The first matching rule applies.
	Capture []CaptureRule
	// HandleParams maps a tool name to one of its arguments whose string value becomes the handle id, cut
	// at 256 bytes after scrubbing. A handle id from Identify takes precedence.
	HandleParams map[string]string
	// Identify, when set, is called for every request. With streamable HTTP, a verified token's subject
	// (TokenInfo.UserID), issuer ("iss" in TokenInfo.Extra) and client id ("client_id" in TokenInfo.Extra)
	// are used when Identify gives no subject.
	Identify func(ctx context.Context, req mcp.Request) Identity
	// NoIdentity suppresses every user identity key: Identify is not called and tokens are not read.
	NoIdentity bool
	// Scrub, when set, runs after the built-in scrubbing rules on every captured argument and result, error
	// message and handle id from HandleParams. It returns the value to send, or false to send nothing. It runs
	// off the request path, on the SDK's export goroutine, and must be safe for concurrent use.
	Scrub func(field, value string) (string, bool)
}

const (
	defaultMaxBytes = 16 << 10
	maxErrorMessage = 2 << 10
	maxHandleID     = 256
)

func checkTransport(t, fallback Transport) (Transport, error) {
	switch t {
	case "":
		return fallback, nil
	case TransportUnknown, TransportStdio, TransportStreamableHTTP, TransportSSE:
		return t, nil
	}
	return "", fmt.Errorf("cruciblemcp: Transport %q is not one of unknown, stdio, streamable_http or sse", t)
}

func checkCapture(rules []CaptureRule) ([]CaptureRule, error) {
	out := make([]CaptureRule, len(rules))
	for i, r := range rules {
		if _, err := path.Match(r.Tool, ""); err != nil {
			return nil, fmt.Errorf("cruciblemcp: Capture[%d].Tool %q is not a valid glob: %w", i, r.Tool, err)
		}
		if r.SampleRate < 0 || r.SampleRate > 1 {
			return nil, fmt.Errorf("cruciblemcp: Capture[%d].SampleRate %v is outside 0 to 1; use 0 or 1 to capture every call", i, r.SampleRate)
		}
		if r.MaxBytes < 0 {
			return nil, fmt.Errorf("cruciblemcp: Capture[%d].MaxBytes is negative; use 0 for the default of 16 KiB", i)
		}
		if r.SampleRate == 0 {
			r.SampleRate = 1
		}
		if r.MaxBytes == 0 {
			r.MaxBytes = defaultMaxBytes
		}
		out[i] = r
	}
	return out, nil
}
