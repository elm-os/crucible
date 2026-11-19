package core

import (
	"context"
	"strings"
	"testing"
	"time"

	"go.opentelemetry.io/otel/attribute"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
	"go.opentelemetry.io/otel/sdk/trace/tracetest"
)

// blockingExporter holds every export until release is closed.
type blockingExporter struct{ release chan struct{} }

func (e *blockingExporter) ExportSpans(context.Context, []sdktrace.ReadOnlySpan) error {
	<-e.release
	return nil
}

func (e *blockingExporter) Shutdown(context.Context) error { return nil }

// The byte bound counts the spans on the queue and in the batch being exported, and drops beyond it.
func TestTheByteBoundDropsBeyondItsCeiling(t *testing.T) {
	stub := tracetest.SpanStub{Name: "s", Attributes: []attribute.KeyValue{attribute.String("note", strings.Repeat("x", 1000))}}
	size := spanBytes(stub.Snapshot())
	exporter := &blockingExporter{release: make(chan struct{})}
	p := NewProcessor(exporter, "", 2048, 3*size, 2, time.Hour)
	for range 5 {
		p.OnEnd(stub.Snapshot())
	}
	if s := p.Stats(); s.QueueFull != 2 || s.QueuedBytes != 3*size {
		t.Fatalf("stats %+v, want 2 dropped and %d bytes queued", s, 3*size)
	}
	close(exporter.release)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := p.Shutdown(ctx); err != nil {
		t.Fatal(err)
	}
	if s := p.Stats(); s.QueuedBytes != 0 || s.Exported != 3 {
		t.Fatalf("after shutdown %+v, want every byte released and 3 spans exported", s)
	}
}
