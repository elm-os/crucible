package core

import (
	"context"
	"slices"
	"testing"
)

// The vectors of the shared conformance registry (contracts 9.4 and 9.6).
func TestHashVectors(t *testing.T) {
	for in, want := range map[string]string{
		"3f8e1c2a-5b7d-4e9f-a1c3-d5e7f9a1b3c5": "84757436d5e0791bd4a8eaa6f1d05986",
		"conformance-session":                  "b1427bda7462603423a9e4a7b73fbdd0",
	} {
		if got := SessionKey(in); got != want {
			t.Errorf("SessionKey(%q) = %s, want %s", in, got, want)
		}
	}
	key := []byte("csk_conformance00000000000000000000000000000000")
	for in, want := range map[string]string{
		"crucible-conformance-request-state-1": "59c50f1c773bc1354d420d974c06c5af",
		"eyJzdGVwIjoiY29uZmlybWVkIn0.c2ln":     "bec039d76aab792bd526d369321fb0c0",
	} {
		if got := stateHash(key, in); got != want {
			t.Errorf("stateHash(%q) = %s, want %s", in, got, want)
		}
	}
}

func TestStateHashIsEmptyBeforeInit(t *testing.T) {
	if got := StateHash("x"); got != "" {
		t.Fatalf("StateHash before Init = %q", got)
	}
}

func TestContextAttrs(t *testing.T) {
	ctx := WithSession(context.Background(), "s")
	ctx = WithUser(ctx, "u")
	ctx = WithTags(ctx, "a", "", "b")
	child := WithTags(ctx, "b", "c")
	if got := attrsFrom(ctx); got.session != "s" || got.user != "u" || !slices.Equal(got.tags, []string{"a", "b"}) {
		t.Errorf("parent attrs = %+v", got)
	}
	if got := attrsFrom(child).tags; !slices.Equal(got, []string{"a", "b", "c"}) {
		t.Errorf("child tags = %v", got)
	}
}

func TestRandomIDIsAVersion4UUID(t *testing.T) {
	id := randomID()
	if len(id) != 36 || id[14] != '4' || id[8] != '-' {
		t.Fatalf("randomID() = %q", id)
	}
	if id == randomID() {
		t.Fatal("two random ids are equal")
	}
}
