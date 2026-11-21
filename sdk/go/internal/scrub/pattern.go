package scrub

import (
	"fmt"
	"strconv"
	"strings"
)

// What a rule's pattern requires of every match, read once when the rules load. Patterns are in the shared subset
// of conformance/scrub-rules.json (literals, escaped punctuation, bracket classes, (?:...) groups, alternation and
// greedy quantifiers), which the conformance driver checks before any SDK loads the file.

// charClass is a bracket class: membership of the ASCII bytes, possibly negated. A byte of a multi-byte UTF-8
// character is never in a positive class.
type charClass struct {
	members [128]bool
	negated bool
}

func (c *charClass) has(b byte) bool { return c.negated != (b < 128 && c.members[b]) }

const unbounded = -1

type item struct {
	char     byte // a literal character, when class and group are nil
	class    *charClass
	group    [][]item
	min, max int // max is unbounded for * and + and {n,}
	start    int // offset in the pattern
}

// lead is a pattern that starts with a literal prefix, then an unbounded repetition of a class that holds the
// prefix's characters, then a literal anchor outside that class: [A-Za-z0-9._%+-]+@... (empty prefix) or
// eyJ[A-Za-z0-9_-]+\.... A match can start only inside the class run that ends at an anchor, and every start in one
// run meets the same anchor, so a matcher needs one anchored attempt per anchor.
type lead struct {
	prefix string
	class  *charClass
	anchor byte
	rest   string // the pattern from the anchor on
}

// pair is a member of a class followed by a literal character: [0-9A-Fa-f] then ':' for the ipv6 rule.
type pair struct {
	class   *charClass
	literal byte
}

// facts are what the scrubber's guards read.
type facts struct {
	minLength int
	literals  []string // per top-level alternative, a literal every match of it contains; nil when one has none
	lead      *lead
	pairs     []pair // per top-level alternative, a pair every match of it holds; nil when one has none
}

// holdsPair reports whether s holds one of the pairs: its literal right after a member of its class. A text
// without any of them holds no match.
func (f *facts) holdsPair(s string) bool {
	for _, p := range f.pairs {
		for at := indexByteFrom(s, p.literal, 1); at >= 0; at = indexByteFrom(s, p.literal, at+1) {
			if p.class.has(s[at-1]) {
				return true
			}
		}
	}
	return false
}

// patternFacts returns the facts of pattern, or none (no guard) when it does not parse as the subset.
func patternFacts(pattern string) facts {
	branches, err := parsePattern(pattern)
	if err != nil {
		return facts{}
	}
	f := facts{minLength: minLength(branches), lead: leadOf(branches, pattern)}
	for _, items := range branches {
		p, ok := pairOf(items)
		if !ok {
			f.pairs = nil
			break
		}
		f.pairs = append(f.pairs, p)
	}
	for _, items := range branches {
		lit, ok := literalOf(items)
		if !ok {
			f.literals = nil
			return f
		}
		f.literals = append(f.literals, lit)
	}
	return f
}

type parser struct {
	p string
	i int
}

func parsePattern(pattern string) ([][]item, error) {
	ps := &parser{p: pattern}
	branches, err := ps.alternation()
	if err == nil && ps.i != len(pattern) {
		err = ps.fail()
	}
	return branches, err
}

func (ps *parser) fail() error {
	return fmt.Errorf("pattern %q is outside the scrub subset at %d", ps.p, ps.i)
}

func (ps *parser) at(k int) byte {
	if ps.i+k < len(ps.p) {
		return ps.p[ps.i+k]
	}
	return 0
}

func unescape(c byte) byte {
	switch c {
	case 'n':
		return '\n'
	case 't':
		return '\t'
	}
	return c
}

func (ps *parser) alternation() ([][]item, error) {
	branches := [][]item{nil}
	for ps.i < len(ps.p) && ps.p[ps.i] != ')' {
		if ps.p[ps.i] == '|' {
			ps.i++
			branches = append(branches, nil)
			continue
		}
		it := item{start: ps.i}
		switch {
		case strings.HasPrefix(ps.p[ps.i:], "(?:"):
			ps.i += 3
			inner, err := ps.alternation()
			if err != nil {
				return nil, err
			}
			if ps.at(0) != ')' {
				return nil, ps.fail()
			}
			ps.i++
			it.group = inner
		case ps.p[ps.i] == '[':
			class, err := ps.charClass()
			if err != nil {
				return nil, err
			}
			it.class = class
		case ps.p[ps.i] == '\\':
			if ps.i+1 >= len(ps.p) {
				return nil, ps.fail()
			}
			it.char = unescape(ps.p[ps.i+1])
			ps.i += 2
		default:
			it.char = ps.p[ps.i]
			ps.i++
		}
		var err error
		if it.min, it.max, err = ps.quantifier(); err != nil {
			return nil, err
		}
		branches[len(branches)-1] = append(branches[len(branches)-1], it)
	}
	return branches, nil
}

func (ps *parser) charClass() (*charClass, error) {
	ps.i++
	c := &charClass{}
	if ps.at(0) == '^' {
		c.negated = true
		ps.i++
	}
	first := ps.i
	for ps.at(0) != ']' || ps.i == first {
		if ps.i >= len(ps.p) {
			return nil, ps.fail()
		}
		low := ps.p[ps.i]
		if low == '\\' {
			ps.i++
			low = unescape(ps.at(0))
		}
		ps.i++
		high := low
		if ps.at(0) == '-' && ps.at(1) != ']' && ps.at(1) != 0 {
			if ps.at(1) == '\\' {
				high = unescape(ps.at(2))
				ps.i += 3
			} else {
				high = ps.at(1)
				ps.i += 2
			}
		}
		for b := int(low); b <= int(high) && b < 128; b++ {
			c.members[b] = true
		}
	}
	ps.i++
	return c, nil
}

func (ps *parser) quantifier() (int, int, error) {
	switch ps.at(0) {
	case '?':
		ps.i++
		return 0, 1, nil
	case '*':
		ps.i++
		return 0, unbounded, nil
	case '+':
		ps.i++
		return 1, unbounded, nil
	case '{':
	default:
		return 1, 1, nil
	}
	end := strings.IndexByte(ps.p[ps.i:], '}')
	if end < 0 {
		return 0, 0, ps.fail()
	}
	body := ps.p[ps.i+1 : ps.i+end]
	ps.i += end + 1
	low, high, ranged := strings.Cut(body, ",")
	lo, err := strconv.Atoi(low)
	if err != nil {
		return 0, 0, ps.fail()
	}
	switch {
	case !ranged:
		return lo, lo, nil
	case high == "":
		return lo, unbounded, nil
	}
	hi, err := strconv.Atoi(high)
	if err != nil {
		return 0, 0, ps.fail()
	}
	return lo, hi, nil
}

func minLength(branches [][]item) int {
	shortest := -1
	for _, items := range branches {
		n := 0
		for _, it := range items {
			size := 1
			if it.group != nil {
				size = minLength(it.group)
			}
			n += it.min * size
		}
		if shortest < 0 || n < shortest {
			shortest = n
		}
	}
	return max(shortest, 0)
}

// single is the byte an item always matches: a literal, or a class with one member.
func single(it item) (byte, bool) {
	switch {
	case it.group != nil:
		return 0, false
	case it.class == nil:
		return it.char, true
	case it.class.negated:
		return 0, false
	}
	found, n := byte(0), 0
	for b, on := range it.class.members {
		if on {
			found, n = byte(b), n+1
		}
	}
	return found, n == 1
}

// literalOf is the longest run of literal characters every match of items contains, if any.
func literalOf(items []item) (string, bool) {
	var runs []string
	var run strings.Builder
	end := func() {
		if run.Len() > 0 {
			runs = append(runs, run.String())
		}
		run.Reset()
	}
	for _, it := range items {
		if c, ok := single(it); ok && it.min >= 1 {
			run.WriteString(strings.Repeat(string(c), it.min))
			if it.max != it.min {
				end()
			}
			continue
		}
		end()
		if it.group != nil && it.min >= 1 && len(it.group) == 1 {
			if inner, ok := literalOf(it.group[0]); ok {
				runs = append(runs, inner)
			}
		}
	}
	end()
	best := ""
	for _, r := range runs {
		if len(r) > len(best) {
			best = r
		}
	}
	return best, best != ""
}

// pairOf is a class item followed by a literal character item, both matched at least once, that every match of
// items holds: the last character the class item matches comes right before the literal. A group matched at least
// once with one alternative holds the pairs of its items.
func pairOf(items []item) (pair, bool) {
	for k := 0; k+1 < len(items); k++ {
		first, second := items[k], items[k+1]
		if first.class != nil && first.min >= 1 && second.class == nil && second.group == nil && second.min >= 1 {
			return pair{class: first.class, literal: second.char}, true
		}
	}
	for _, it := range items {
		if it.group != nil && it.min >= 1 && len(it.group) == 1 {
			if p, ok := pairOf(it.group[0]); ok {
				return p, true
			}
		}
	}
	return pair{}, false
}

// once is the character of an item that is one literal character, matched once.
func once(items []item, k int) (byte, bool) {
	if k >= len(items) {
		return 0, false
	}
	it := items[k]
	if it.group != nil || it.class != nil || it.min != 1 || it.max != 1 {
		return 0, false
	}
	return it.char, true
}

func leadOf(branches [][]item, pattern string) *lead {
	if len(branches) != 1 {
		return nil
	}
	items := branches[0]
	k := 0
	var prefix strings.Builder
	for c, ok := once(items, k); ok; c, ok = once(items, k) {
		prefix.WriteByte(c)
		k++
	}
	anchor, ok := once(items, k+1)
	if k >= len(items) || !ok {
		return nil
	}
	repeated := items[k]
	if repeated.class == nil || repeated.min < 1 || repeated.max != unbounded || repeated.class.has(anchor) {
		return nil
	}
	p := prefix.String()
	for i := range len(p) {
		if !repeated.class.has(p[i]) {
			return nil
		}
	}
	return &lead{prefix: p, class: repeated.class, anchor: anchor, rest: pattern[items[k+1].start:]}
}
