package cruciblemcp

import (
	"strings"
	"testing"

	"github.com/elm-os/crucible/sdk/go/internal/scrub"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

// A large result is serialized no further than the cap and the scrub lookahead need.
func TestALargeResultIsSerializedNoFurtherThanTheCapNeeds(t *testing.T) {
	image := make([]byte, 1<<20)
	results := []*mcp.CallToolResult{
		{Content: []mcp.Content{&mcp.TextContent{Text: strings.Repeat("x", 5<<20)}}},
		{Content: []mcp.Content{&mcp.ImageContent{Data: image, MIMEType: "image/png"}}},
		{Content: []mcp.Content{&mcp.EmbeddedResource{Resource: &mcp.ResourceContents{URI: "file:///a", Text: strings.Repeat("é", 1<<20)}}}},
		{StructuredContent: map[string]any{"rows": make([]any, 100_000)}},
	}
	serialized := 0
	SetWorkHook(func(sz, _ int) { serialized = max(serialized, sz) })
	defer SetWorkHook(nil)
	s := &server{}
	for _, r := range results {
		s.capturedResult(r, 16384)
	}
	if bound := scrub.WindowSize(16384); serialized > bound {
		t.Fatalf("serialized %d bytes for one result, more than %d", serialized, bound)
	}
}
