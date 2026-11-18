// Package crucible sends traces from Go agents to a self-hosted Crucible server: Init, Flush and Shutdown for
// the exporter, context helpers for the session, user and tags of a trace, and Observe for spans with input,
// output, model, usage and cost. Package cruciblemcp instruments MCP servers and clients.
package crucible

import "github.com/elm-os/crucible/sdk/go/internal/core"

// Version is the version of this SDK. It is sent in the User-Agent of every export request.
const Version = core.Version
