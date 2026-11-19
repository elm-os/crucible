package cruciblemcp

import (
	"encoding/json"
	"math/rand/v2"
	"strings"
	"testing"

	"github.com/elm-os/crucible/sdk/go/internal/scrub"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

// resultText is the whole captured form of a tool result, which resultWindow writes a prefix of.
func resultText(r *mcp.CallToolResult) (string, error) {
	b, err := json.Marshal(struct {
		Content           []mcp.Content `json:"content"`
		StructuredContent any           `json:"structuredContent,omitempty"`
		IsError           bool          `json:"isError,omitempty"`
	}{r.Content, r.StructuredContent, r.IsError})
	return string(b), err
}

// pieces of strings json.Marshal treats differently: escapes, HTML characters, multi-byte characters, U+2028
// and invalid UTF-8.
var pieces = []string{"a", "Z", "0", " ", "\"", "\\", "\n", "\t", "\x01", "<", ">", "&", "é", "中", "😀", " ", "\xff", "\xe2\x82"}

func randomText(r *rand.Rand, longest int) string {
	var b strings.Builder
	for range r.IntN(longest + 1) {
		b.WriteString(pieces[r.IntN(len(pieces))])
	}
	return b.String()
}

func randomValue(r *rand.Rand, depth int) any {
	switch r.IntN(map[bool]int{true: 6, false: 9}[depth > 2]) {
	case 0:
		return nil
	case 1:
		return r.IntN(2) == 0
	case 2:
		return []any{1.5, -2e21, 1e-7, 42.0, json.Number("17")}[r.IntN(5)]
	case 3:
		return randomText(r, 8)
	case 4:
		return randomText(r, 400)
	case 5:
		return map[string]any(nil)
	case 6:
		items := make([]any, r.IntN(5))
		for i := range items {
			items[i] = randomValue(r, depth+1)
		}
		return items
	case 7:
		return map[string]int{"n": r.IntN(9)}
	}
	m := map[string]any{}
	for range r.IntN(5) {
		m[randomText(r, 6)] = randomValue(r, depth+1)
	}
	return m
}

func randomBlock(r *rand.Rand) mcp.Content {
	data := make([]byte, r.IntN(900))
	for i := range data {
		data[i] = byte(r.IntN(256))
	}
	var meta mcp.Meta
	if r.IntN(3) == 0 {
		meta = mcp.Meta{"k": randomText(r, 5)}
	}
	var annotations *mcp.Annotations
	if r.IntN(3) == 0 {
		annotations = &mcp.Annotations{Audience: []mcp.Role{"user"}, Priority: 0.5}
	}
	switch r.IntN(8) {
	case 0:
		return nil
	case 1:
		return &mcp.ImageContent{Data: data, MIMEType: randomText(r, 3), Meta: meta, Annotations: annotations}
	case 2:
		return &mcp.AudioContent{Data: data, MIMEType: "audio/wav"}
	case 3:
		return &mcp.AudioContent{Data: data, MIMEType: randomText(r, 3), Annotations: annotations}
	case 4:
		res := &mcp.ResourceContents{URI: "file:///" + randomText(r, 4), MIMEType: randomText(r, 2), Text: randomText(r, 300)}
		if r.IntN(2) == 0 {
			res.Blob = data
		}
		return &mcp.EmbeddedResource{Resource: res, Meta: meta, Annotations: annotations}
	case 5:
		return &mcp.ResourceLink{URI: "file:///x", Name: randomText(r, 40), Title: randomText(r, 3), Description: randomText(r, 200), Meta: meta}
	case 6:
		// Not a nil *mcp.TextContent: json.Marshal of the whole result panics on one (Go 1.27).
		return &mcp.TextContent{}
	}
	return &mcp.TextContent{Text: randomText(r, 600), Meta: meta, Annotations: annotations}
}

// The window resultWindow writes is the prefix scrub.Prefix cuts from the whole captured form, for any limit.
func TestTheResultWindowEqualsTheWholeFormFollowedByTheCut(t *testing.T) {
	r := rand.New(rand.NewPCG(18112026, 7))
	for range 3000 {
		result := &mcp.CallToolResult{IsError: r.IntN(3) == 0}
		if r.IntN(8) > 0 {
			result.Content = make([]mcp.Content, r.IntN(5))
			for i := range result.Content {
				result.Content[i] = randomBlock(r)
			}
		}
		if r.IntN(2) == 0 {
			result.StructuredContent = randomValue(r, 0)
		}
		full, err := resultText(result)
		if err != nil {
			t.Fatal(err)
		}
		for _, limit := range []int{0, 1, r.IntN(64), r.IntN(2000), 1 << 20} {
			w := resultWindow(result, limit)
			want, cut := scrub.Prefix(full, limit)
			if string(w.buf) != want || w.more != cut || w.err != nil {
				t.Fatalf("limit %d: window %q (more %v, err %v), want %q (more %v)", limit, w.buf, w.more, w.err, want, cut)
			}
		}
	}
}

// A large result is serialized no further than the cap and the scrub lookahead need, give or take the last piece
// written: the cap plus 8 KiB, the scenario's work bound.
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
	if bound := 16384 + 8192; serialized > bound {
		t.Fatalf("serialized %d bytes for one result, more than %d", serialized, bound)
	}
}
