// Package scrub removes secrets and personal data from text before it leaves the process (contract 9.10).
//
// The rules are the shared conformance/scrub-rules.json, copied byte for byte into rules.json because a Go
// module cannot embed files outside its directory; a test keeps the two identical. Rules run in file order, each
// replacing every non-overlapping match, leftmost first. After a match its validator rejects, the search resumes
// after the match's first character that is neither an ASCII letter nor a digit, or after the match when it has
// none. Go's regexp package matches in time linear in the input.
package scrub

import (
	_ "embed"
	"encoding/json"
	"fmt"
	"regexp"
	"strings"
	"unicode/utf8"
)

//go:embed rules.json
var rulesJSON []byte

type rule struct {
	name        string
	re          *regexp.Regexp
	luhn        bool
	replacement string
}

type ruleFile struct {
	Rules []struct {
		Name        string `json:"name"`
		Pattern     string `json:"pattern"`
		Replacement string `json:"replacement"`
		Validator   string `json:"validator"`
	} `json:"rules"`
}

var rules = mustParse(rulesJSON)

func parse(data []byte) ([]rule, error) {
	var f ruleFile
	if err := json.Unmarshal(data, &f); err != nil {
		return nil, err
	}
	out := make([]rule, 0, len(f.Rules))
	for _, r := range f.Rules {
		re, err := regexp.Compile(r.Pattern)
		if err != nil {
			return nil, fmt.Errorf("rule %s: %w", r.Name, err)
		}
		if r.Validator != "" && r.Validator != "luhn" {
			return nil, fmt.Errorf("rule %s: unknown validator %q", r.Name, r.Validator)
		}
		out = append(out, rule{name: r.Name, re: re, luhn: r.Validator == "luhn", replacement: r.Replacement})
	}
	return out, nil
}

func mustParse(data []byte) []rule {
	rs, err := parse(data)
	if err != nil {
		panic("scrub: embedded rules.json is invalid: " + err.Error())
	}
	return rs
}

// String returns s with every rule applied in file order.
func String(s string) string {
	for i := range rules {
		s = rules[i].apply(s)
	}
	return s
}

func (r *rule) apply(s string) string {
	var b strings.Builder
	last, pos := 0, 0
	for pos <= len(s) {
		loc := r.re.FindStringIndex(s[pos:])
		if loc == nil {
			break
		}
		start, end := pos+loc[0], pos+loc[1]
		if r.luhn && !luhn(s[start:end]) {
			pos = resume(s, start, end)
			continue
		}
		b.WriteString(s[last:start])
		b.WriteString(r.replacement)
		last = end
		if end == start {
			end++
		}
		pos = end
	}
	if last == 0 {
		return s
	}
	b.WriteString(s[last:])
	return b.String()
}

// resume is where the search continues after a rejected match s[start:end]: after its first character that is
// neither an ASCII letter nor a digit, or after the match when it has none.
func resume(s string, start, end int) int {
	for i := start; i < end; i++ {
		c := s[i]
		if !('0' <= c && c <= '9' || 'a' <= c && c <= 'z' || 'A' <= c && c <= 'Z') {
			return i + 1
		}
	}
	if end == start {
		return end + 1
	}
	return end
}

// luhn reports whether the digits of s pass the Luhn checksum. Characters other than digits are skipped.
func luhn(s string) bool {
	sum, n := 0, 0
	for i := len(s) - 1; i >= 0; i-- {
		c := s[i]
		if c < '0' || c > '9' {
			continue
		}
		d := int(c - '0')
		if n%2 == 1 {
			d *= 2
			if d > 9 {
				d -= 9
			}
		}
		sum += d
		n++
	}
	return n > 0 && sum%10 == 0
}

// lookahead is how far past the cap Capped scrubs, so that a secret which starts before the cap and ends after
// it is still recognised whole and its visible prefix replaced.
const lookahead = 1024

// Capped returns at most max bytes of s, scrubbed, ending on a character boundary. Only the first max+lookahead
// bytes of s are read, so the cost is bounded by the cap and not by the size of s. truncated reports whether
// anything was cut.
func Capped(s string, max int) (out string, truncated bool) {
	window, cut := Prefix(s, max+lookahead)
	out, cut2 := Prefix(String(window), max)
	return out, cut || cut2
}

// WindowSize is how many bytes Capped reads for the cap max.
func WindowSize(max int) int { return max + lookahead }

// Window returns, as a string, the part of b that Capped reads for the cap max, and whether b is longer. Only
// that part is copied.
func Window(b []byte, max int) (string, bool) {
	if limit := max + lookahead; len(b) > limit {
		return string(b[:limit]), true
	}
	return string(b), false
}

// Prefix returns the longest prefix of s that is at most max bytes and ends on a character boundary, and
// whether it is shorter than s.
func Prefix(s string, max int) (string, bool) {
	if len(s) <= max {
		return s, false
	}
	if max < 0 {
		max = 0
	}
	end := max
	for end > 0 && !utf8.RuneStart(s[end]) {
		end--
	}
	return s[:end], true
}
