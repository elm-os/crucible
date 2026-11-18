package core

import (
	"context"
	"slices"
)

type attrsKey struct{}

// attrs are the trace-level values a context carries to every span started under it (contract 3.3: SDKs set
// session and user ids at span start so that every span carries them).
type attrs struct {
	session, user string
	tags          []string
}

func attrsFrom(ctx context.Context) attrs {
	a, _ := ctx.Value(attrsKey{}).(attrs)
	return a
}

// WithSession returns a context whose spans carry the session id.
func WithSession(ctx context.Context, id string) context.Context {
	a := attrsFrom(ctx)
	a.session = id
	return context.WithValue(ctx, attrsKey{}, a)
}

// WithUser returns a context whose spans carry the user id.
func WithUser(ctx context.Context, id string) context.Context {
	a := attrsFrom(ctx)
	a.user = id
	return context.WithValue(ctx, attrsKey{}, a)
}

// WithTags returns a context whose spans carry the tags, added to those it already carries.
func WithTags(ctx context.Context, tags ...string) context.Context {
	a := attrsFrom(ctx)
	merged := slices.Clone(a.tags)
	for _, t := range tags {
		if t != "" && !slices.Contains(merged, t) {
			merged = append(merged, t)
		}
	}
	a.tags = merged
	return context.WithValue(ctx, attrsKey{}, a)
}
