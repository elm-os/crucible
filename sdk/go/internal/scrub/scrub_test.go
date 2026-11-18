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

func TestVectors(t *testing.T) {
	var f struct {
		Vectors []struct {
			Input  string `json:"input"`
			Output string `json:"output"`
		} `json:"vectors"`
	}
	if err := json.Unmarshal(rulesJSON, &f); err != nil {
		t.Fatal(err)
	}
	if len(f.Vectors) == 0 {
		t.Fatal("rules.json has no vectors")
	}
	for _, v := range f.Vectors {
		if got := String(v.Input); got != v.Output {
			t.Errorf("String(%q)\n got %q\nwant %q", v.Input, got, v.Output)
		}
	}
}

func TestEveryRuleHasAVector(t *testing.T) {
	var f struct {
		Vectors []struct {
			Input string `json:"input"`
		} `json:"vectors"`
	}
	if err := json.Unmarshal(rulesJSON, &f); err != nil {
		t.Fatal(err)
	}
	for _, r := range rules {
		hit := false
		for _, v := range f.Vectors {
			if r.apply(v.Input) != v.Input {
				hit = true
				break
			}
		}
		if !hit {
			t.Errorf("no vector exercises rule %s", r.name)
		}
	}
}

func TestParseRefusesBadRules(t *testing.T) {
	for _, data := range []string{
		`{"rules":[{"name":"x","pattern":"(","replacement":"r"}]}`,
		`{"rules":[{"name":"x","pattern":"a","group":1,"replacement":"r"}]}`,
		`{"rules":[{"name":"x","pattern":"a","check":"crc","replacement":"r"}]}`,
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
		"4111 1111 1111 1111",
		"+44 20 7946 0958",
		"198.51.100.23",
		"AKIAABCDEFGHIJKLMNOP",
	}
	const filler = "abcxyz ,;:"
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

// When the shared rules land in conformance/scrub-rules.json, this copy must be identical to them.
func TestMatchesTheSharedRulesWhenPresent(t *testing.T) {
	shared, err := os.ReadFile("../../../../conformance/scrub-rules.json")
	if os.IsNotExist(err) {
		t.Skip("conformance/scrub-rules.json does not exist yet")
	}
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(bytes.TrimSpace(shared), bytes.TrimSpace(rulesJSON)) {
		t.Fatal("internal/scrub/rules.json differs from conformance/scrub-rules.json; copy the shared file here")
	}
}

func FuzzString(f *testing.F) {
	f.Add("ada@example.com")
	f.Add(`{"password":"x`)
	f.Add("-----BEGIN KEY-----")
	f.Add("4111 1111 1111 1111")
	f.Fuzz(func(t *testing.T, s string) {
		out := String(s)
		if utf8.ValidString(s) && !utf8.ValidString(out) {
			t.Fatalf("String(%q) = %q, not valid UTF-8", s, out)
		}
	})
}
