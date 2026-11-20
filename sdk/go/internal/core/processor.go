package core

import (
	"context"
	"sync"
	"sync/atomic"
	"time"

	"go.opentelemetry.io/otel/attribute"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
)

// Span keys the processor sets on every span at start (contract 10.6).
const (
	KeySessionID = "crucible.session.id"
	KeyUserID    = "crucible.user.id"
	KeyTags      = "crucible.tags"
	KeyRelease   = "crucible.release"
)

// Stats counts what happened to finished spans and to scores.
type Stats struct {
	Exported     uint64
	QueueFull    uint64
	ExportFailed uint64
	// QueuedBytes is the estimated size of the queued spans and of the batch being exported (see spanBytes).
	QueuedBytes int64
	// FeedbackSent, FeedbackDropped and FeedbackFailed count scores (see FeedbackSender).
	FeedbackSent, FeedbackDropped, FeedbackFailed uint64
}

// spanOverheadBytes is what a span costs beyond its strings: ids, times, kind, status code, links and the
// resource and scope references.
const spanOverheadBytes = 256

// spanBytes estimates a span's size from lengths already known: its name, attributes, events and status message,
// plus a fixed allowance. The attribute count limit bounds the work.
func spanBytes(s sdktrace.ReadOnlySpan) int64 {
	n := spanOverheadBytes + len(s.Name()) + len(s.Status().Description) + attributeBytes(s.Attributes())
	for _, e := range s.Events() {
		n += len(e.Name) + attributeBytes(e.Attributes)
	}
	return int64(n)
}

func attributeBytes(kvs []attribute.KeyValue) int {
	n := 0
	for _, kv := range kvs {
		n += len(kv.Key)
		switch kv.Value.Type() {
		case attribute.STRING:
			n += len(kv.Value.AsString())
		case attribute.STRINGSLICE:
			for _, v := range kv.Value.AsStringSlice() {
				n += len(v)
			}
		default:
			n += 8
		}
	}
	return n
}

// queued is a span on the queue with its estimated size.
type queued struct {
	span sdktrace.ReadOnlySpan
	size int64
}

// Processor is the SDK's span processor. At span start it copies the session, user, tags and metadata carried in the
// context onto the span. At span end it puts the span on a queue bounded by spans and by estimated bytes, without
// blocking: when either bound is reached the span is dropped and counted. One goroutine drains the queue in
// batches to the exporter.
type Processor struct {
	exporter   sdktrace.SpanExporter
	release    string
	queueBytes int64 // bound on queuedBytes
	batchSize  int
	interval   time.Duration

	queue   chan queued
	flushes chan chan error
	stop    chan struct{}
	done    chan struct{}

	// exportCtx is cancelled when a shutdown runs out of time, which abandons an export in progress.
	exportCtx    context.Context
	cancelExport context.CancelFunc
	stopOnce     sync.Once

	exported, queueFull, exportFailed atomic.Uint64
	queuedBytes                       atomic.Int64
}

var _ sdktrace.SpanProcessor = (*Processor)(nil)

// NewProcessor starts a processor whose queue holds queueSize spans and queueBytes estimated bytes, the batch being
// exported included.
func NewProcessor(exporter sdktrace.SpanExporter, release string, queueSize int, queueBytes int64, batchSize int, interval time.Duration) *Processor {
	ctx, cancel := context.WithCancel(context.Background())
	p := &Processor{
		exporter:     exporter,
		release:      release,
		queueBytes:   queueBytes,
		batchSize:    batchSize,
		interval:     interval,
		queue:        make(chan queued, queueSize),
		flushes:      make(chan chan error),
		stop:         make(chan struct{}),
		done:         make(chan struct{}),
		exportCtx:    ctx,
		cancelExport: cancel,
	}
	go p.run()
	return p
}

// OnStart copies the context's session, user, tags and metadata, and the configured release, onto the span.
func (p *Processor) OnStart(parent context.Context, s sdktrace.ReadWriteSpan) {
	a := attrsFrom(parent)
	if a.session != "" {
		s.SetAttributes(attribute.String(KeySessionID, a.session))
	}
	if a.user != "" {
		s.SetAttributes(attribute.String(KeyUserID, a.user))
	}
	if len(a.tags) > 0 {
		s.SetAttributes(attribute.StringSlice(KeyTags, a.tags))
	}
	s.SetAttributes(a.metadata...)
	if p.release != "" {
		s.SetAttributes(attribute.String(KeyRelease, p.release))
	}
}

// OnEnd queues the span, or drops and counts it when the queue holds queueSize spans or would pass queueBytes. It
// never blocks.
func (p *Processor) OnEnd(s sdktrace.ReadOnlySpan) {
	size := spanBytes(s)
	for {
		queued := p.queuedBytes.Load()
		if queued+size > p.queueBytes {
			p.queueFull.Add(1)
			return
		}
		if p.queuedBytes.CompareAndSwap(queued, queued+size) {
			break
		}
	}
	select {
	case p.queue <- queued{span: s, size: size}:
	default:
		p.queuedBytes.Add(-size)
		p.queueFull.Add(1)
	}
}

// ForceFlush exports every span queued before the call. It returns the export error, if any, or the context's
// error when ctx ends first.
func (p *Processor) ForceFlush(ctx context.Context) error {
	reply := make(chan error, 1)
	select {
	case p.flushes <- reply:
	case <-p.done:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
	select {
	case err := <-reply:
		return err
	case <-ctx.Done():
		return ctx.Err()
	}
}

// Shutdown exports what is queued and stops the processor and its exporter. When ctx ends first, the export
// in progress is abandoned and the context's error is returned.
func (p *Processor) Shutdown(ctx context.Context) error {
	p.stopOnce.Do(func() { close(p.stop) })
	select {
	case <-p.done:
	case <-ctx.Done():
		p.cancelExport()
		return ctx.Err()
	}
	return p.exporter.Shutdown(ctx)
}

// Stats returns the counters.
func (p *Processor) Stats() Stats {
	return Stats{
		Exported:     p.exported.Load(),
		QueueFull:    p.queueFull.Load(),
		ExportFailed: p.exportFailed.Load(),
		QueuedBytes:  p.queuedBytes.Load(),
	}
}

func (p *Processor) run() {
	defer close(p.done)
	defer p.cancelExport()
	ticker := time.NewTicker(p.interval)
	defer ticker.Stop()
	batch := make([]sdktrace.ReadOnlySpan, 0, p.batchSize)
	var batchBytes int64
	var failed error
	export := func() {
		if len(batch) == 0 {
			return
		}
		if err := p.exporter.ExportSpans(p.exportCtx, batch); err != nil {
			p.exportFailed.Add(uint64(len(batch)))
			failed = err
		} else {
			p.exported.Add(uint64(len(batch)))
		}
		p.queuedBytes.Add(-batchBytes)
		clear(batch)
		batch, batchBytes = batch[:0], 0
	}
	add := func(q queued) {
		batch = append(batch, q.span)
		batchBytes += q.size
		if len(batch) >= p.batchSize {
			export()
		}
	}
	drain := func() {
		for {
			select {
			case s := <-p.queue:
				add(s)
			default:
				export()
				return
			}
		}
	}
	for {
		select {
		case s := <-p.queue:
			add(s)
		case <-ticker.C:
			export()
		case reply := <-p.flushes:
			failed = nil
			drain()
			reply <- failed
		case <-p.stop:
			drain()
			return
		}
	}
}
