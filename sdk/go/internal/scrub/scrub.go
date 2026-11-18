// Package scrub removes secrets and personal data from text before it leaves the process (contract 9.10).
//
// The rules are data in rules.json, compiled with Go's regexp package, whose matching is linear in the input
// size. A rule replaces the whole match, or only one capture group when it names one, so the key in
// `"password": "..."` survives and only the value is replaced. A rule with the luhn check replaces a match
// only when its digits pass the Luhn checksum.
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
	group       int
	luhn        bool
	replacement string
}

type ruleFile struct {
	Rules []struct {
		Name        string `json:"name"`
		Pattern     string `json:"pattern"`
		Group       int    `json:"group"`
		Check       string `json:"check"`
		Replacement string `json:"replacement"`
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
		if r.Group < 0 || r.Group > re.NumSubexp() {
			return nil, fmt.Errorf("rule %s: group %d does not exist", r.Name, r.Group)
		}
		if r.Check != "" && r.Check != "luhn" {
			return nil, fmt.Errorf("rule %s: unknown check %q", r.Name, r.Check)
		}
		out = append(out, rule{name: r.Name, re: re, group: r.Group, luhn: r.Check == "luhn", replacement: r.Replacement})
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

// String returns s with every match of every rule replaced, applying the rules in file order.
func String(s string) string {
	for i := range rules {
		s = rules[i].apply(s)
	}
	return s
}

func (r *rule) apply(s string) string {
	matches := r.re.FindAllStringSubmatchIndex(s, -1)
	if matches == nil {
		return s
	}
	var b strings.Builder
	last := 0
	for _, m := range matches {
		start, end := m[2*r.group], m[2*r.group+1]
		if start < 0 || (r.luhn && !luhn(s[m[0]:m[1]])) {
			continue
		}
		b.WriteString(s[last:start])
		b.WriteString(r.replacement)
		last = end
	}
	if last == 0 && b.Len() == 0 {
		return s
	}
	b.WriteString(s[last:])
	return b.String()
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

// lookahead is how far past the cap Capped scrubs, so that a secret which starts before the cap and ends
// after it is still recognised whole and its visible prefix replaced.
const lookahead = 1024

// Capped returns at most max bytes of s, scrubbed, ending on a character boundary. Only the first
// max+lookahead bytes of s are read, so the cost is bounded by the cap and not by the size of s. truncated
// reports whether anything was cut.
func Capped(s string, max int) (out string, truncated bool) {
	window, cut := Prefix(s, max+lookahead)
	out, cut2 := Prefix(String(window), max)
	return out, cut || cut2
}

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
