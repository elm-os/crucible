// Feedback (contract 12.6): a score with source `feedback` on a trace, a span or a session, posted to
// `POST {apiUrl}/api/v1/projects/{projectId}/feedback` with the project secret key, which needs the `write` scope
// (contract 11.3). `feedback()` only queues the score, so the request leaves the caller's path as spans do
// (contract 9.10). One sender posts the queue in call order, one request at a time, because the latest score of a
// name wins. Before it posts, it flushes the span queue: the route answers 404 for a trace Crucible has not
// received, so the spans that ended before the call go first. A failed connection, a timeout, 408, 429, 5xx and a
// `conflict` 409 are retried, three attempts in all, after `Retry-After` (at most 10 s) or else 1 s and 2 s, with
// the same body and Idempotency-Key, so Crucible writes the score once. Nothing is thrown to the caller: a score
// that cannot be queued counts as dropped, one that Crucible refuses or that fails every attempt as failed, and the
// first of each is reported once as a process warning (code CRUCIBLE_FEEDBACK) that says what to change.

import { randomBytes } from "node:crypto";
import { setTimeout as sleep } from "node:timers/promises";
import { context } from "@opentelemetry/api";
import { suppressTracing } from "@opentelemetry/core";
import { within } from "./queue.ts";

/** A score on a trace (`traceId`), a span (`traceId` and `spanId`) or a session (`sessionId`). */
export interface Feedback {
  /** Score name, for example `helpful`, without `#`. A later score of the same name (and `scoreId`) replaces it. */
  name: string;
  /** A number (`numeric`), a boolean (`boolean`) or a string (`categorical`, or `text` with `dataType`). */
  value: number | string | boolean;
  /** The trace, 32 hex characters. */
  traceId?: string;
  /** A span of the trace, 16 hex characters. */
  spanId?: string;
  /** A session, instead of a trace. */
  sessionId?: string;
  /** Keeps scores of one name apart, for example one per reviewer. */
  scoreId?: string;
  /** How Crucible reads `value`; by default from its type. */
  dataType?: "numeric" | "boolean" | "categorical" | "text";
  /** At most 4 KiB. */
  comment?: string;
  /** A JSON object, at most 16 KiB. */
  metadata?: Record<string, unknown>;
  /** Removes the score of this name (and `scoreId`) from the target. */
  retracted?: boolean;
  /**
   * Sent as `Idempotency-Key`: 1 to 128 characters of `A-Z`, `a-z`, `0-9`, `_` and `-`. Crucible writes a score
   * once per key and body for 24 hours. Default: a random key per call.
   */
  idempotencyKey?: string;
}

/** Feedback counters since `init`. */
export interface FeedbackStats {
  /** Scores Crucible accepted. */
  feedbackSent: number;
  /** Scores never sent: the queue was full, no project id is set, or `shutdown` ran out of time before them. */
  feedbackDropped: number;
  /** Scores Crucible refused, or that failed on every attempt. */
  feedbackFailed: number;
}

export interface SenderOptions {
  /** The project's feedback route; `undefined` without a project id. */
  url: string | undefined;
  secretKey: string;
  userAgent: string;
  /** Bound on one request, and on `flush` and `shutdown`. */
  timeoutMs: number;
  /** Exports the spans queued so far; never rejects. */
  flushSpans: () => Promise<void>;
}

/** Scores waiting to be sent, the one being sent included. */
const QUEUE_LIMIT = 1_024;
const ATTEMPTS = 3;
/** The waits before the second and third attempts when the answer has no `Retry-After`. */
const BACKOFF_MS = [1_000, 2_000];
const RETRY_AFTER_CAP_MS = 10_000;

interface Item {
  seq: number;
  body: string;
  key: string;
}

const SETTINGS =
  "scores go to apiUrl (CRUCIBLE_API_URL), the API listener, under projectId (CRUCIBLE_PROJECT_ID), the key's project, with a secret key that has the write scope";

/** How one request ended: accepted, or not and why, and whether another attempt may succeed and after what wait. */
type Outcome =
  | { sent: true }
  | { sent: false; reason: string; retry: false }
  | { sent: false; reason: string; retry: true; retryAfterMs: number | undefined };

export class FeedbackSender {
  readonly #options: SenderOptions;
  readonly #abort = new AbortController();
  #queue: Item[] = [];
  #seq = 0;
  /** Items with a sequence number up to this one were queued before the last span flush started. */
  #flushed = 0;
  #running = false;
  #done: Promise<void> = Promise.resolve();
  #closed = false;
  #stopped = false;
  #sent = 0;
  #dropped = 0;
  #failed = 0;
  #warnedDrop = false;
  #warnedFailure = false;

  constructor(options: SenderOptions) {
    this.#options = options;
  }

  stats(): FeedbackStats {
    return {
      feedbackSent: this.#sent,
      feedbackDropped: this.#dropped,
      feedbackFailed: this.#failed,
    };
  }

  /** Queues one score, or drops and counts it. Never throws. */
  add(feedback: Feedback): void {
    if (this.#closed) {
      this.#dropped += 1;
      return;
    }
    if (this.#options.url === undefined) {
      this.#drop(
        "Crucible feedback() has no project id, so the score was dropped: pass the projectId option to init() or set CRUCIBLE_PROJECT_ID to the key's project (`crucible admin create-key` prints it as project_id).",
      );
      return;
    }
    if (this.#queue.length >= QUEUE_LIMIT) {
      this.#drop(
        `Crucible feedback() queue is full (${QUEUE_LIMIT} scores), so scores are being dropped; check that ${this.#options.url} answers.`,
      );
      return;
    }
    let body: string;
    try {
      body = JSON.stringify(wire(feedback));
    } catch (error) {
      // Metadata that is not JSON (a cycle, a bigint) cannot be sent.
      this.#drop(
        `Crucible feedback() could not encode the score as JSON (${String(error)}), so it was dropped.`,
      );
      return;
    }
    this.#seq += 1;
    this.#queue.push({
      seq: this.#seq,
      body,
      key: feedback.idempotencyKey ?? randomBytes(16).toString("hex"),
    });
    if (!this.#running) {
      this.#running = true;
      this.#done = this.#drain();
    }
  }

  /** Resolves when the scores queued before the call are sent, or after `timeoutMs`. */
  flush(): Promise<void> {
    return within(this.#done, this.#options.timeoutMs);
  }

  /** Sends what is queued within `budgetMs`, then stops; the scores still queued count as dropped. */
  async shutdown(budgetMs: number): Promise<void> {
    this.#closed = true;
    await within(this.#done, budgetMs);
    this.#stopped = true;
    this.#abort.abort();
  }

  #drop(message: string): void {
    this.#dropped += 1;
    if (!this.#warnedDrop) {
      this.#warnedDrop = true;
      process.emitWarning(message, { code: "CRUCIBLE_FEEDBACK" });
    }
  }

  async #drain(): Promise<void> {
    try {
      while (this.#queue.length > 0) {
        if (this.#stopped) {
          this.#dropped += this.#queue.length;
          this.#queue = [];
          break;
        }
        const item = this.#queue[0] as Item;
        if (item.seq > this.#flushed) {
          this.#flushed = this.#seq;
          await this.#options.flushSpans();
        }
        const failure = await this.#post(item);
        if (failure === undefined) {
          this.#sent += 1;
        } else if (this.#stopped) {
          this.#dropped += 1;
        } else {
          this.#failed += 1;
          if (!this.#warnedFailure) {
            this.#warnedFailure = true;
            process.emitWarning(`Crucible feedback() was not sent: ${failure}; ${SETTINGS}.`, {
              code: "CRUCIBLE_FEEDBACK",
            });
          }
        }
        this.#queue.shift();
      }
    } finally {
      // Runs in the same turn as the loop's last check, so a score queued later starts a new drain.
      this.#running = false;
    }
  }

  /** Sends one score, retrying what may succeed later; `undefined` when Crucible accepted it, else why not. */
  async #post(item: Item): Promise<string | undefined> {
    let reason = "";
    let wait = 0;
    for (let attempt = 0; attempt < ATTEMPTS; attempt += 1) {
      if (attempt > 0) {
        try {
          await sleep(wait, undefined, { signal: this.#abort.signal, ref: false });
        } catch {
          return reason;
        }
      }
      const outcome = await this.#request(item);
      if (outcome.sent) {
        return undefined;
      }
      reason = outcome.reason;
      if (!outcome.retry) {
        return reason;
      }
      wait = outcome.retryAfterMs ?? BACKOFF_MS[attempt] ?? 0;
    }
    return reason;
  }

  async #request(item: Item): Promise<Outcome> {
    const { url, secretKey, userAgent, timeoutMs } = this.#options;
    try {
      const signal = AbortSignal.any([this.#abort.signal, AbortSignal.timeout(timeoutMs)]);
      // The SDK's own request must not produce spans in an instrumented application.
      const response = await context.with(suppressTracing(context.active()), () =>
        fetch(url as string, {
          method: "POST",
          headers: {
            authorization: `Bearer ${secretKey}`,
            "content-type": "application/json",
            "idempotency-key": item.key,
            "user-agent": userAgent,
          },
          body: item.body,
          // Following a redirect would send the secret key to another URL and turn the post into a GET.
          redirect: "manual",
          signal,
        }),
      );
      const text = await response.text();
      if (response.ok) {
        return { sent: true };
      }
      const reason = `${url} answered ${response.status}${detail(text)}`;
      if (!retryable(response.status, text)) {
        return { sent: false, reason, retry: false };
      }
      const wait = retryAfter(response.headers.get("retry-after"));
      return { sent: false, reason, retry: true, retryAfterMs: wait };
    } catch (error) {
      const reason = `${url} could not be reached (${String(error)})`;
      return { sent: false, reason, retry: true, retryAfterMs: undefined };
    }
  }
}

/** The route's request body: its snake_case fields, only those set. */
function wire(feedback: Feedback): Record<string, unknown> {
  const fields: [string, unknown][] = [
    ["trace_id", feedback.traceId],
    ["span_id", feedback.spanId],
    ["session_id", feedback.sessionId],
    ["name", feedback.name],
    ["score_id", feedback.scoreId],
    ["value", feedback.value],
    ["data_type", feedback.dataType],
    ["comment", feedback.comment],
    ["metadata", feedback.metadata],
    ["retracted", feedback.retracted],
  ];
  const body: Record<string, unknown> = {};
  for (const [key, value] of fields) {
    if (value !== undefined) {
      body[key] = value;
    }
  }
  return body;
}

/** The `code` and `detail` of a problem body. */
function problem(body: string): { code?: unknown; detail?: unknown } {
  try {
    const parsed: unknown = JSON.parse(body);
    return typeof parsed === "object" && parsed !== null ? parsed : {};
  } catch {
    return {};
  }
}

function detail(body: string): string {
  const { code, detail: text } = problem(body);
  const parts = [code, text].filter((part): part is string => typeof part === "string");
  return parts.length > 0 ? ` (${parts.join(": ")})` : "";
}

/** Whether an answer may change on a later attempt: a timeout, throttling, a server fault or a key in flight. */
function retryable(status: number, body: string): boolean {
  if (status === 408 || status === 429 || status >= 500) {
    return true;
  }
  // `conflict`: a request with this key is still running. `idempotency_key_reused` never changes.
  return status === 409 && problem(body).code === "conflict";
}

function retryAfter(header: string | null): number | undefined {
  const seconds = header === null || header.trim() === "" ? Number.NaN : Number(header);
  return Number.isFinite(seconds) && seconds >= 0
    ? Math.min(seconds * 1_000, RETRY_AFTER_CAP_MS)
    : undefined;
}
