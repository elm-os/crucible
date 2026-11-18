package scrub

import (
	"bytes"
	"encoding/json"
	"math/rand/v2"
	"os"
	"strings"
	"testing"
	"unicode/utf8"
)

type vectors struct {
	Rules []struct {
		Name     string   `json:"name"`
		Positive []string `json:"positive"`
		Negative []string `json:"negative"`
	} `json:"rules"`
	Examples []struct {
		Input  string `json:"input"`
		Output string `json:"output"`
	} `json:"examples"`
}

func loadVectors(t *testing.T) vectors {
	t.Helper()
	var v vectors
	if err := json.Unmarshal(rulesJSON, &v); err != nil {
		t.Fatal(err)
	}
	if len(v.Rules) != len(rules) {
		t.Fatalf("%d rules with vectors, %d compiled", len(v.Rules), len(rules))
	}
	return v
}

// Every rule changes its positive inputs and leaves its negative ones alone, on its own.
func TestRuleVectors(t *testing.T) {
	v := loadVectors(t)
	for i, r := range v.Rules {
		if len(r.Positive) == 0 || len(r.Negative) == 0 {
			t.Errorf("rule %s lacks positive or negative inputs", r.Name)
		}
		for _, in := range r.Positive {
			if rules[i].apply(in) == in {
				t.Errorf("rule %s left its positive input %q unchanged", r.Name, in)
			}
		}
		for _, in := range r.Negative {
			if out := rules[i].apply(in); out != in {
				t.Errorf("rule %s changed its negative input %q to %q", r.Name, in, out)
			}
		}
	}
}

// Each example is the output of the whole rule list.
func TestExamples(t *testing.T) {
	v := loadVectors(t)
	if len(v.Examples) == 0 {
		t.Fatal("no examples")
	}
	for _, e := range v.Examples {
		if got := String(e.Input); got != e.Output {
			t.Errorf("String(%q)\n got %q\nwant %q", e.Input, got, e.Output)
		}
	}
}

// After a rejected match, the search resumes after the match's first separator, so a card that starts at a
// later group of a rejected run is still found.
func TestResumeAfterRejection(t *testing.T) {
	card := rules[ruleIndex(t, "card")]
	if got := card.apply("0134 4111 1111 1111 1111"); got != "0134 [redacted:card]" {
		t.Fatalf("card rule gave %q", got)
	}
	if got := resume("4111-x", 0, 6); got != 5 {
		t.Fatalf("resume = %d, want 5", got)
	}
	if got := resume("41111111", 0, 8); got != 8 {
		t.Fatalf("resume without a separator = %d, want 8", got)
	}
}

func ruleIndex(t *testing.T, name string) int {
	t.Helper()
	for i := range rules {
		if rules[i].name == name {
			return i
		}
	}
	t.Fatalf("no rule %s", name)
	return 0
}

func TestParseRefusesBadRules(t *testing.T) {
	for _, data := range []string{
		`{"rules":[{"name":"x","pattern":"(","replacement":"r"}]}`,
		`{"rules":[{"name":"x","pattern":"a","validator":"crc","replacement":"r"}]}`,
	} {
		if _, err := parse([]byte(data)); err == nil {
			t.Errorf("parse(%s) accepted invalid rules", data)
		}
	}
}

func TestLuhn(t *testing.T) {
	for s, want := range map[string]bool{
		"4111111111111111":    true,
		"4111 1111 1111 1111": true,
		"5500-0000-0000-0004": true,
		"4111111111111112":    false,
		"":                    false,
	} {
		if got := luhn(s); got != want {
			t.Errorf("luhn(%q) = %v, want %v", s, got, want)
		}
	}
}

// Planted secrets are removed wherever they sit in surrounding text that cannot form a secret itself.
func TestPlantedSecretsAreRemoved(t *testing.T) {
	secrets := []string{
		"ada@example.com",
		"csk_0123456789abcdefghijABCDEFGHIJ0123456789abc",
		"eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ1In0.c2ln",
		"Bearer 9f8e7d6c5b4a39281706f5e4d3c2b1a0",
		"4111 1111 1111 1111",
		"+44 20 7946 0958",
		"198.51.100.23",
		"2001:db8::8a2e:370:7334",
	}
	const filler = "abcxyz ,;"
	r := rand.New(rand.NewPCG(1, 2))
	word := func() string {
		b := make([]byte, r.IntN(40))
		for i := range b {
			b[i] = filler[r.IntN(len(filler))]
		}
		return string(b)
	}
	for i := 0; i < 2000; i++ {
		secret := secrets[r.IntN(len(secrets))]
		in := word() + " " + secret + " " + word()
		if out := String(in); strings.Contains(out, secret) {
			t.Fatalf("String(%q) = %q still holds %q", in, out, secret)
		}
	}
}

func TestCappedStaysWithinTheCap(t *testing.T) {
	r := rand.New(rand.NewPCG(3, 4))
	alphabet := []rune("ab@.c 1é中-")
	for i := 0; i < 2000; i++ {
		n := r.IntN(200)
		var b strings.Builder
		for j := 0; j < n; j++ {
			b.WriteRune(alphabet[r.IntN(len(alphabet))])
		}
		s := b.String()
		max := r.IntN(80)
		out, truncated := Capped(s, max)
		if len(out) > max {
			t.Fatalf("Capped(%q, %d) = %q, longer than the cap", s, max, out)
		}
		if !utf8.ValidString(out) {
			t.Fatalf("Capped(%q, %d) = %q, not valid UTF-8", s, max, out)
		}
		if len(s) > max && !truncated {
			t.Fatalf("Capped(%q, %d) cut the input but did not report it", s, max)
		}
	}
}

// A secret that straddles the cap is replaced before the cut, so no prefix of it survives.
func TestCappedScrubsAcrossTheCap(t *testing.T) {
	secret := "csk_0123456789abcdefghijABCDEFGHIJ0123456789abc"
	s := strings.Repeat("a", 30) + " " + secret
	out, truncated := Capped(s, 40)
	if !truncated {
		t.Fatal("not reported as truncated")
	}
	if strings.Contains(out, "csk_0123") {
		t.Fatalf("Capped kept a prefix of the secret: %q", out)
	}
}

// rules.json is a copy of the shared conformance/scrub-rules.json and must stay identical to it.
func TestRulesAreTheSharedFile(t *testing.T) {
	shared, err := os.ReadFile("../../../../conformance/scrub-rules.json")
	if os.IsNotExist(err) {
		t.Skip("conformance/scrub-rules.json is not in this checkout")
	}
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(shared, rulesJSON) {
		t.Fatal("internal/scrub/rules.json differs from conformance/scrub-rules.json; copy the shared file here")
	}
}

func FuzzString(f *testing.F) {
	f.Add("ada@example.com")
	f.Add("-----BEGIN KEY-----")
	f.Add("4111 1111 1111 1111")
	f.Add("+1 415 555 0134 4111 1111 1111 1111")
	f.Fuzz(func(t *testing.T, s string) {
		out := String(s)
		if utf8.ValidString(s) && !utf8.ValidString(out) {
			t.Fatalf("String(%q) = %q, not valid UTF-8", s, out)
		}
	})
}
