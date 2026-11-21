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
	"slices"
	"strings"
	"sync/atomic"
	"unicode/utf8"
)

//go:embed rules.json
var rulesJSON []byte

type rule struct {
	name        string
	re          *regexp.Regexp
	luhn        bool
	replacement string
	facts       facts
	rest        *regexp.Regexp // for a lead pattern without a validator: the pattern from its anchor on, anchored
}

// matcherHook, when set, is called once per matcher call with the rule's name and the call's steps (see
// searchSteps). The scrubbing work checks read it.
var matcherHook atomic.Pointer[func(rule string, steps int)]

// RuleCount is the number of rules.
func RuleCount() int { return len(rules) }

// SetMatcherHook installs, or with nil removes, the matcher hook.
func SetMatcherHook(h func(rule string, steps int)) {
	if h == nil {
		matcherHook.Store(nil)
		return
	}
	matcherHook.Store(&h)
}

func (r *rule) searched(s string, from, to int) {
	if h := matcherHook.Load(); h != nil {
		(*h)(r.name, searchSteps(r.facts.lead, s, from, to))
	}
}

// searchSteps is the steps of an unanchored search that tried the starts from from up to to, as a backtracking
// matcher takes them at the least: one per start, and from a start where a lead pattern's prefix matches, the
// prefix and the whole class run after it, which the greedy repetition consumes before it backtracks.
func searchSteps(l *lead, s string, from, to int) int {
	if l == nil {
		return to - from
	}
	p := len(l.prefix)
	end := min(len(s), to+p)
	for end < len(s) && l.class.has(s[end]) {
		end++
	}
	runs := make([]int, end-from+1)
	for k := end - 1; k >= from; k-- {
		if l.class.has(s[k]) {
			runs[k-from] = runs[k-from+1] + 1
		}
	}
	steps := 0
	for i := from; i < to; i++ {
		steps++
		if strings.HasPrefix(s[i:], l.prefix) && i+p-from < len(runs) {
			steps += p + runs[i+p-from]
		}
	}
	return steps
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
		f := patternFacts(r.Pattern)
		var rest *regexp.Regexp
		if f.lead != nil && r.Validator == "" {
			if rest, err = regexp.Compile("^(?:" + f.lead.rest + ")"); err != nil {
				return nil, fmt.Errorf("rule %s: %w", r.Name, err)
			}
		}
		out = append(out, rule{
			name:        r.Name,
			re:          re,
			luhn:        r.Validator == "luhn",
			replacement: r.Replacement,
			facts:       f,
			rest:        rest,
		})
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

// shortest is the length no rule matches below.
var shortest = func() int {
	n := -1
	for i := range rules {
		if n < 0 || rules[i].facts.minLength < n {
			n = rules[i].facts.minLength
		}
	}
	return max(n, 0)
}()

// String returns s with every rule applied in file order.
func String(s string) string {
	if len(s) < shortest {
		return s
	}
	for i := range rules {
		s = rules[i].apply(s)
	}
	return s
}

// apply is the rule behind guards that take time linear in s and change nothing: text shorter than any match,
// without the rule's required literal, or without the class and literal pair every match holds, is left alone, and
// a lead pattern is tried once per anchor.
func (r *rule) apply(s string) string {
	f := &r.facts
	if len(s) < f.minLength {
		return s
	}
	if f.literals != nil && !slices.ContainsFunc(f.literals, func(lit string) bool { return strings.Contains(s, lit) }) {
		return s
	}
	if f.pairs != nil {
		// One search for the pair, which a matcher takes at most one step per start for.
		if h := matcherHook.Load(); h != nil {
			(*h)(r.name, len(s))
		}
		if !f.holdsPair(s) {
			return s
		}
	}
	if r.rest != nil {
		return r.applyLead(s)
	}
	return r.applyPlain(s)
}

// applyLead finds a lead pattern's matches from its anchors. A match ends its class run at an anchor, and every
// start in that run meets the same anchor, so only the run's leftmost possible start is a candidate and the rest
// of the pattern is tried once, anchored there. Each position is scanned a bounded number of times.
func (r *rule) applyLead(s string) string {
	l := r.facts.lead
	var b strings.Builder
	last, floor := 0, 0
	at := indexByteFrom(s, l.anchor, 0)
	for at >= 0 {
		start := at
		for start > floor && l.class.has(s[start-1]) {
			start--
		}
		begin := -1
		if l.prefix == "" {
			if start < at {
				begin = start
			}
		} else if k := strings.Index(s[start:at], l.prefix); k >= 0 && start+k+len(l.prefix) < at {
			begin = start + k
		}
		if begin >= 0 {
			loc := r.rest.FindStringIndex(s[at:])
			r.attempted(s, at, loc)
			if loc != nil {
				b.WriteString(s[last:begin])
				b.WriteString(r.replacement)
				last, floor = at+loc[1], at+loc[1]
				at = indexByteFrom(s, l.anchor, floor)
				continue
			}
		}
		at = indexByteFrom(s, l.anchor, at+1)
	}
	if last == 0 {
		return s
	}
	b.WriteString(s[last:])
	return b.String()
}

// attempted reports an anchored attempt at at to the matcher hook: the bytes it can read, which are its match,
// or the anchor, the class run after it and the byte that ends the run.
func (r *rule) attempted(s string, at int, loc []int) {
	h := matcherHook.Load()
	if h == nil {
		return
	}
	if loc != nil {
		(*h)(r.name, loc[1])
		return
	}
	end := at + 1
	for end < len(s) && r.facts.lead.class.has(s[end]) {
		end++
	}
	(*h)(r.name, end-at+1)
}

func indexByteFrom(s string, c byte, from int) int {
	if from > len(s) {
		return -1
	}
	if i := strings.IndexByte(s[from:], c); i >= 0 {
		return from + i
	}
	return -1
}

// applyPlain is the rule without its guards: the reference the differential tests hold apply to.
func (r *rule) applyPlain(s string) string {
	var b strings.Builder
	last, pos := 0, 0
	for pos <= len(s) {
		loc := r.re.FindStringIndex(s[pos:])
		if loc == nil {
			r.searched(s, pos, len(s))
			break
		}
		start, end := pos+loc[0], pos+loc[1]
		r.searched(s, pos, start+1)
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
