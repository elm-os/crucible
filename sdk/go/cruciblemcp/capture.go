package cruciblemcp

import (
	"encoding/json"
	"maps"
	"slices"
	"unicode/utf8"

	"github.com/elm-os/crucible/sdk/go/internal/scrub"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

// window is a JSON text written up to a byte limit: the prefix scrub.Prefix would cut from what json.Marshal
// writes for the whole value.
type window struct {
	buf      []byte
	limit    int
	more     bool  // the whole text is longer than the limit
	produced int   // bytes produced, including those past the limit
	err      error // a value that does not marshal
}

func (w *window) room() int { return w.limit - len(w.buf) }

// write appends b and reports whether there is room for more. At the limit it cuts b on a character boundary, as
// scrub.Prefix would cut the whole text.
func (w *window) write(b []byte) bool {
	w.produced += len(b)
	room := w.room()
	if len(b) <= room {
		w.buf = append(w.buf, b...)
		return true
	}
	end := room
	for end > 0 && !utf8.RuneStart(b[end]) {
		end--
	}
	w.buf = append(w.buf, b[:end]...)
	w.more = true
	return false
}

// need is how many bytes of a field the window can still use: the room left, and the bytes after it that decide
// where a character boundary falls.
func (w *window) need() int { return w.room() + utf8.UTFMax }

// resultWindow writes the captured form of r, which is json.Marshal of its content, structured content and error
// flag, up to limit bytes. It serializes no more than that needs: content blocks are marshaled one at a time, each
// with any string or data field longer than the room left cut first, and structured content made of maps, slices
// and strings is walked.
func resultWindow(r *mcp.CallToolResult, limit int) *window {
	w := &window{limit: limit}
	_ = w.write([]byte(`{"content":`)) && w.blocks(r.Content) && w.structured(r.StructuredContent) &&
		(!r.IsError || w.write([]byte(`,"isError":true`))) && w.write([]byte("}"))
	return w
}

func (w *window) blocks(content []mcp.Content) bool {
	if content == nil {
		return w.write([]byte("null"))
	}
	if !w.write([]byte("[")) {
		return false
	}
	for i, c := range content {
		if i > 0 && !w.write([]byte(",")) {
			return false
		}
		b, err := marshalBlock(c, w.need())
		if err != nil {
			w.err = err
			return false
		}
		if !w.write(b) {
			return false
		}
	}
	return w.write([]byte("]"))
}

// marshalBlock marshals a content block as json.Marshal does inside a slice, after cutting every string and data
// field to what need bytes of output can show: the cut copy's text agrees with the whole block's up to there.
func marshalBlock(c mcp.Content, need int) ([]byte, error) {
	cut := func(s string) string {
		p, _ := scrub.Prefix(s, need)
		return p
	}
	// Base64 of a prefix whose length is a multiple of 3 is a prefix of the whole base64 text.
	cutData := func(b []byte) []byte {
		if k := 3 * ((need + 3) / 4); len(b) > k {
			return b[:k]
		}
		return b
	}
	if isNilBlock(c) {
		return []byte("null"), nil
	}
	switch c := c.(type) {
	case *mcp.TextContent:
		cp := *c
		cp.Text = cut(c.Text)
		return json.Marshal(&cp)
	case *mcp.ImageContent:
		cp := *c
		cp.Data, cp.MIMEType = cutData(c.Data), cut(c.MIMEType)
		return json.Marshal(&cp)
	case *mcp.AudioContent:
		cp := *c
		cp.Data, cp.MIMEType = cutData(c.Data), cut(c.MIMEType)
		return json.Marshal(cp)
	case *mcp.EmbeddedResource:
		cp := *c
		if c.Resource != nil {
			res := *c.Resource
			res.URI, res.MIMEType, res.Text, res.Blob = cut(res.URI), cut(res.MIMEType), cut(res.Text), cutData(res.Blob)
			cp.Resource = &res
		}
		return json.Marshal(&cp)
	case *mcp.ResourceLink:
		cp := *c
		cp.URI, cp.Name, cp.Title = cut(c.URI), cut(c.Name), cut(c.Title)
		cp.Description, cp.MIMEType = cut(c.Description), cut(c.MIMEType)
		return json.Marshal(&cp)
	}
	return json.Marshal(c)
}

// isNilBlock reports a nil block, or a nil pointer to one, which is written as null.
func isNilBlock(c mcp.Content) bool {
	switch c := c.(type) {
	case nil:
		return true
	case *mcp.TextContent:
		return c == nil
	case *mcp.ImageContent:
		return c == nil
	case *mcp.AudioContent:
		return c == nil
	case *mcp.EmbeddedResource:
		return c == nil
	case *mcp.ResourceLink:
		return c == nil
	}
	return false
}

func (w *window) structured(v any) bool {
	if v == nil {
		return true
	}
	return w.write([]byte(`,"structuredContent":`)) && w.value(v)
}

// value writes v as json.Marshal does: maps of strings to values in key order, slices and strings walked within
// the window, anything else marshaled whole.
func (w *window) value(v any) bool {
	switch v := v.(type) {
	case string:
		return w.str(v)
	case map[string]any:
		if v == nil {
			return w.write([]byte("null"))
		}
		if !w.write([]byte("{")) {
			return false
		}
		for i, k := range slices.Sorted(maps.Keys(v)) {
			if (i > 0 && !w.write([]byte(","))) || !w.str(k) || !w.write([]byte(":")) || !w.value(v[k]) {
				return false
			}
		}
		return w.write([]byte("}"))
	case []any:
		if v == nil {
			return w.write([]byte("null"))
		}
		if !w.write([]byte("[")) {
			return false
		}
		for i, item := range v {
			if (i > 0 && !w.write([]byte(","))) || !w.value(item) {
				return false
			}
		}
		return w.write([]byte("]"))
	}
	b, err := json.Marshal(v)
	if err != nil {
		w.err = err
		return false
	}
	return w.write(b)
}

// str writes a JSON string; one longer than the window can use is cut first and written without its closing
// quote, which is a prefix of the whole string's text.
func (w *window) str(s string) bool {
	if need := w.need(); len(s) > need {
		p, _ := scrub.Prefix(s, need)
		b, _ := json.Marshal(p)
		return w.write(b[:len(b)-1])
	}
	b, _ := json.Marshal(s)
	return w.write(b)
}
