package scrub

import (
	"encoding/base64"
	"math/rand/v2"
	"reflect"
	"strings"
	"testing"
	"time"
)

func run(unit string, n int) string { return strings.Repeat(unit, n/len(unit)+1)[:n] }

// stepInputs make a backtracking matcher slow on the shared rules, about 16 KiB each.
func stepInputs() map[string]string {
	bytes := make([]byte, 12288)
	for i := range bytes {
		bytes[i] = byte(i % 256)
	}
	return map[string]string{
		"letters":             strings.Repeat("a", 16384),
		"digits":              strings.Repeat("1", 16384),
		"digit pairs":         strings.Repeat("1 ", 8192),
		"at signs":            strings.Repeat("a@", 8192),
		"dots":                strings.Repeat("a.", 8192),
		"one at sign":         strings.Repeat("a", 8000) + "@" + strings.Repeat("a", 8000),
		"hex":                 run("0123456789abcdef", 16384),
		"alphanumeric":        run("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789", 16384),
		"base64":              base64.StdEncoding.EncodeToString(bytes),
		"pem header":          "-----BEGIN A-----" + strings.Repeat("A", 16000),
		"jwt prefixes":        strings.Repeat("eyJ", 5461),
		"jwt chain":           strings.Repeat("eyJ", 5000) + strings.Repeat(".eyJ", 400),
		"bearer":              strings.Repeat("bearer ", 2340),
		"colons":              strings.Repeat("1:", 8192),
		"plus ones":           strings.Repeat("+1", 8192),
		"phone groups":        strings.Repeat("(415) ", 2730),
		"secret key prefixes": strings.Repeat("csk_", 4096),
	}
}

// matcherSteps is the matcher steps of scrubbing s with the whole rule list.
func matcherSteps(s string) int {
	steps := 0
	SetMatcherHook(func(_ string, n int) { steps += n })
	defer SetMatcherHook(nil)
	String(s)
	return steps
}

type ruleFacts struct {
	minLength int
	literals  []string
	lead      []string
}

func TestTheRulesPatternsYieldTheFactsTheGuardsUse(t *testing.T) {
	got := map[string]ruleFacts{}
	for _, r := range rules {
		f := ruleFacts{minLength: r.facts.minLength, literals: r.facts.literals}
		if l := r.facts.lead; l != nil {
			f.lead = []string{l.prefix, string(l.anchor)}
		}
		got[r.name] = f
	}
	want := map[string]ruleFacts{
		"pem":        {32, []string{"-----BEGIN "}, nil},
		"jwt":        {7, []string{"eyJ"}, []string{"eyJ", "."}},
		"bearer":     {23, nil, nil},
		"secret_key": {12, []string{"csk_"}, nil},
		"card":       {14, nil, nil},
		"email":      {6, []string{"@"}, []string{"", "@"}},
		"ipv6":       {6, []string{":", ":"}, nil},
		"ipv4":       {7, []string{"."}, nil},
		"phone":      {8, nil, nil},
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("facts\n got %v\nwant %v", got, want)
	}
}

func TestMatcherStepsStayLinearInTheLength(t *testing.T) {
	for name, s := range stepInputs() {
		if steps, bound := matcherSteps(s), 2*len(rules)*len(s); steps > bound {
			t.Errorf("%s: %d matcher steps > %d", name, steps, bound)
		}
	}
}

// fragments are pieces of matches, near misses and separators that the generated text is built from.
var fragments = []string{
	" ", "a", "0", "7", "@", ".", ":", "-", "_", "+", "(", ")", "/", "=", "%", "\n", "é", "😀",
	"eyJ", "csk_", "bearer ", "Bearer\t", "-----BEGIN ", "-----END ", "-----", "KEY", "::", "@@",
	"a@b", "a@b.c", "x@y.zz", "ada.lovelace@example.com", "ops+alerts@mail.example.co.uk",
	"+1 415 555 0134", "(415) 555-0134", "4111 1111 1111 1111", "4111-1111-1111-1112", "378282246310005",
	"Bearer 9f8e7d6c5b4a39281706f5e4d3c2b1a0", "csk_7Hq2mXr9TzP4vLw8NcB1dKf6YsA3eJu5RgQ0hVt2iWo",
	"203.0.113.42", "2001:db8::8a2e:370:7334", "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJhZGEifQ.c2lnbmF0dXJl",
	"eyJa.b", "eyJeyJ.x.", "eyJ.", "-----BEGIN KEY-----\nMIIB\n-----END KEY-----",
}

var runUnits = []string{"a", "Z", "9", "f", ".", "-", "_", "+", "%", "ab@", "eyJ", "1 "}

func generated(r *rand.Rand) string {
	var b strings.Builder
	for range r.IntN(14) {
		if r.Float64() < 0.7 {
			b.WriteString(fragments[r.IntN(len(fragments))])
		} else {
			b.WriteString(strings.Repeat(runUnits[r.IntN(len(runUnits))], 1+r.IntN(40)))
		}
	}
	return b.String()
}

func TestGuardedApplicationEqualsPlainApplicationOverGeneratedText(t *testing.T) {
	r := rand.New(rand.NewPCG(20261118, 1))
	for range 3000 {
		s := generated(r)
		plain := s
		for i := range rules {
			if got, want := rules[i].apply(s), rules[i].applyPlain(s); got != want {
				t.Fatalf("rule %s on %q: guarded %q, plain %q", rules[i].name, s, got, want)
			}
			plain = rules[i].applyPlain(plain)
		}
		if got := String(s); got != plain {
			t.Fatalf("all rules on %q: guarded %q, plain %q", s, got, plain)
		}
		if steps, bound := matcherSteps(s), 2*len(rules)*max(len(s), 1); steps > bound {
			t.Fatalf("%q: %d matcher steps > %d", s, steps, bound)
		}
	}
}

func TestALongRunScrubsWithinAGenerousCeiling(t *testing.T) {
	s := strings.Repeat("a", 65536) + "@" + strings.Repeat("a", 65536)
	start := time.Now()
	String(s)
	if elapsed := time.Since(start); elapsed > 2*time.Second {
		t.Fatalf("scrubbing took %v", elapsed)
	}
}
