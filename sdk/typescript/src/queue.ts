// The export queue (contract 9.10): spans leave the request path through a bounded in-memory queue that a
// background loop drains in batches. The bound counts the batch being exported too. A full queue drops the span
// and counts it; ending a span never blocks.

import { context } from "@opentelemetry/api";
import { ExportResultCode, suppressTracing } from "@opentelemetry/core";
import type { ReadableSpan, SpanExporter, SpanProcessor } from "@opentelemetry/sdk-trace-base";

/** Counters of the export queue since `init`. */
export interface QueueStats {
  /** Spans waiting for export or being exported. */
  queued: number;
  /** Spans dropped because the queue was full, or because they ended after `shutdown`. */
  dropped: number;
  /** Spans the collector accepted. */
  exported: number;
  /** Spans in batches whose export failed or timed out. */
  failed: number;
}

export interface QueueOptions {
  maxQueueSize: number;
  maxBatchSize: number;
  flushIntervalMs: number;
  /** Bound on one export, and on the whole of `forceFlush` and `shutdown`. */
  timeoutMs: number;
}

/** Extra time an export may take beyond its own timeout before the queue stops waiting for it. */
const EXPORT_GRACE_MS = 1_000;

export class QueueSpanProcessor implements SpanProcessor {
  readonly #exporter: SpanExporter;
  readonly #options: QueueOptions;
  #queue: ReadableSpan[] = [];
  #inFlight: Promise<void> | undefined;
  #inFlightSpans = 0;
  #timer: NodeJS.Timeout | undefined;
  #closed = false;
  #dropped = 0;
  #exported = 0;
  #failed = 0;

  constructor(exporter: SpanExporter, options: QueueOptions) {
    this.#exporter = exporter;
    this.#options = options;
  }

  stats(): QueueStats {
    return {
      queued: this.#queue.length + this.#inFlightSpans,
      dropped: this.#dropped,
      exported: this.#exported,
      failed: this.#failed,
    };
  }

  onStart(): void {}

  onEnd(span: ReadableSpan): void {
    if (this.#closed || this.#queue.length + this.#inFlightSpans >= this.#options.maxQueueSize) {
      this.#dropped += 1;
      return;
    }
    this.#queue.push(span);
    if (this.#queue.length >= this.#options.maxBatchSize) {
      this.#send();
    } else {
      this.#arm();
    }
  }

  forceFlush(): Promise<void> {
    return within(this.#drain(), this.#options.timeoutMs);
  }

  async shutdown(): Promise<void> {
    if (this.#closed) {
      return;
    }
    this.#closed = true;
    await within(
      (async () => {
        await this.#drain();
        await this.#exporter.shutdown();
      })(),
      this.#options.timeoutMs,
    );
    clearTimeout(this.#timer);
  }

  async #drain(): Promise<void> {
    while (this.#queue.length > 0 || this.#inFlight !== undefined) {
      this.#send();
      await this.#inFlight;
    }
  }

  #arm(): void {
    if (this.#timer !== undefined || this.#inFlight !== undefined) {
      return;
    }
    this.#timer = setTimeout(() => {
      this.#timer = undefined;
      this.#send();
    }, this.#options.flushIntervalMs);
    this.#timer.unref();
  }

  /** Starts exporting one batch unless an export is already in flight. */
  #send(): void {
    if (this.#inFlight !== undefined || this.#queue.length === 0) {
      return;
    }
    clearTimeout(this.#timer);
    this.#timer = undefined;
    const batch = this.#queue.splice(0, this.#options.maxBatchSize);
    this.#inFlightSpans = batch.length;
    this.#inFlight = this.#export(batch).then(() => {
      this.#inFlight = undefined;
      this.#inFlightSpans = 0;
      if (this.#queue.length >= this.#options.maxBatchSize) {
        this.#send();
      } else if (this.#queue.length > 0) {
        this.#arm();
      }
    });
  }

  #export(batch: ReadableSpan[]): Promise<void> {
    return new Promise((resolve) => {
      let settled = false;
      const settle = (ok: boolean): void => {
        if (settled) {
          return;
        }
        settled = true;
        clearTimeout(guard);
        if (ok) {
          this.#exported += batch.length;
        } else {
          this.#failed += batch.length;
        }
        resolve();
      };
      const guard = setTimeout(() => settle(false), this.#options.timeoutMs + EXPORT_GRACE_MS);
      guard.unref();
      try {
        // The exporter's own HTTP request must not produce spans.
        context.with(suppressTracing(context.active()), () => {
          this.#exporter.export(batch, (result) =>
            settle(result.code === ExportResultCode.SUCCESS),
          );
        });
      } catch {
        settle(false);
      }
    });
  }
}

/** Resolves when `work` settles or after `ms`, whichever is first. Never rejects. */
async function within(work: Promise<void>, ms: number): Promise<void> {
  let timer: NodeJS.Timeout | undefined;
  const deadline = new Promise<void>((resolve) => {
    timer = setTimeout(resolve, ms);
    timer.unref();
  });
  try {
    await Promise.race([work.catch(() => undefined), deadline]);
  } finally {
    clearTimeout(timer);
  }
}
