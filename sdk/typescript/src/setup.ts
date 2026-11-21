// SDK setup: one private tracer provider that exports OTLP/HTTP protobuf to Crucible's `/v1/traces`, and the
// feedback sender that posts scores to the project's feedback route.
// The provider is never registered globally, so an application's own OpenTelemetry setup is left alone.
// Every option is passed explicitly, so `OTEL_*` variables cannot redirect the export, replace its Bearer key or
// turn sampling off; the OTLP exporter still adds any other headers named in `OTEL_EXPORTER_OTLP_HEADERS`.

import { randomUUID } from "node:crypto";
import { type Attributes, type Tracer, trace } from "@opentelemetry/api";
import { OTLPTraceExporter } from "@opentelemetry/exporter-trace-otlp-proto";
import { defaultResource, resourceFromAttributes } from "@opentelemetry/resources";
import {
  AlwaysOnSampler,
  BasicTracerProvider,
  type SpanProcessor,
} from "@opentelemetry/sdk-trace-base";
import { type Feedback, FeedbackSender, type FeedbackStats } from "./feedback.ts";
import { QueueSpanProcessor, type QueueStats } from "./queue.ts";

export const SDK_NAME = "@elm-os/crucible";
export const SDK_VERSION = "0.1.1";
const USER_AGENT = `crucible-sdk-typescript/${SDK_VERSION}`;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export interface InitOptions {
  /** Base URL of a Crucible OTLP listener. Default: `CRUCIBLE_ENDPOINT`, else `http://localhost:4318`. */
  endpoint?: string;
  /**
   * Project secret key, sent as a Bearer token: the `ingest` scope sends spans, the `write` scope feedback. New keys
   * have both. Default: `CRUCIBLE_SECRET_KEY`.
   */
  secretKey?: string;
  /**
   * The id of the key's project (a UUID), which `feedback` needs: `crucible admin create-key` prints it as
   * `project_id`, and the UI shows it as `project=` in the address bar. Default: `CRUCIBLE_PROJECT_ID`.
   */
  projectId?: string;
  /**
   * Base URL of the Crucible API listener, where `feedback` posts. Default: `CRUCIBLE_API_URL`, else `endpoint`
   * without `/v1/traces`. The API listener (port 8080) also takes spans; the OTLP listener (port 4318) does not
   * serve the API.
   */
  apiUrl?: string;
  /** Resource `service.name`. */
  serviceName?: string;
  /** Resource `service.version`. */
  serviceVersion?: string;
  /** Resource `deployment.environment.name`. */
  environment?: string;
  /** `crucible.release` on every span. */
  release?: string;
  /** Resource `crucible.deploy.id` (contract 9.7). When absent, Crucible uses the server version. */
  deployId?: string;
  /** Spans the export queue holds before it drops. Default 2,048. */
  queueSize?: number;
  /** Bytes of spans, estimated from their strings, the export queue holds before it drops. Default 8 MiB. */
  queueBytes?: number;
  /** Longest wait before a partial batch is exported. Default 1,000 ms. */
  flushIntervalMs?: number;
  /** Bound on one export request, and on `flush` and `shutdown`. Default 10,000 ms. */
  exportTimeoutMs?: number;
}

interface State {
  provider: BasicTracerProvider;
  tracer: Tracer;
  queue: QueueSpanProcessor;
  feedback: FeedbackSender;
  secretKey: string;
  timeoutMs: number;
}

/** Counters since `init`: the span export queue and feedback. */
export interface Stats extends QueueStats, FeedbackStats {}

let state: State | undefined;

/**
 * Starts exporting to Crucible. Throws on a missing key, a malformed endpoint or API URL, or a project id that is
 * not a UUID, naming the setting to fix.
 */
export function init(options: InitOptions = {}): void {
  if (state !== undefined) {
    throw new Error(
      "Crucible init() was already called; call shutdown() before calling init() again.",
    );
  }
  const key = options.secretKey ?? process.env.CRUCIBLE_SECRET_KEY ?? "";
  if (key === "") {
    throw new Error(
      "Crucible init() needs a project secret key with the ingest scope: pass the secretKey option or set CRUCIBLE_SECRET_KEY.",
    );
  }
  const url = tracesUrl(
    options.endpoint ?? process.env.CRUCIBLE_ENDPOINT ?? "http://localhost:4318",
  );
  const feedbackUrl = feedbackRoute(
    options.apiUrl ?? process.env.CRUCIBLE_API_URL,
    url,
    options.projectId ?? process.env.CRUCIBLE_PROJECT_ID,
  );
  const timeoutMs = positive(options.exportTimeoutMs, 10_000, "exportTimeoutMs");
  const queue = new QueueSpanProcessor(
    new OTLPTraceExporter({
      url,
      headers: { Authorization: `Bearer ${key}` },
      timeoutMillis: timeoutMs,
      userAgent: USER_AGENT,
    }),
    {
      maxQueueSize: positive(options.queueSize, 2_048, "queueSize"),
      maxQueueBytes: positive(options.queueBytes, 8 * 1024 * 1024, "queueBytes"),
      maxBatchSize: 512,
      flushIntervalMs: positive(options.flushIntervalMs, 1_000, "flushIntervalMs"),
      timeoutMs,
    },
  );
  const spanProcessors: SpanProcessor[] = [queue];
  const release = options.release;
  if (release !== undefined) {
    spanProcessors.unshift({
      onStart: (span) => void span.setAttribute("crucible.release", release),
      onEnd: () => {},
      forceFlush: async () => {},
      shutdown: async () => {},
    });
  }
  const provider = new BasicTracerProvider({
    resource: defaultResource().merge(resourceFromAttributes(resourceAttributes(options))),
    sampler: new AlwaysOnSampler(),
    spanLimits: { attributeCountLimit: 128, attributeValueLengthLimit: Number.POSITIVE_INFINITY },
    spanProcessors,
  });
  const sender = new FeedbackSender({
    url: feedbackUrl,
    secretKey: key,
    userAgent: USER_AGENT,
    timeoutMs,
    flushSpans: () => provider.forceFlush().catch(() => undefined),
  });
  state = {
    provider,
    tracer: provider.getTracer(SDK_NAME, SDK_VERSION),
    queue,
    feedback: sender,
    secretKey: key,
    timeoutMs,
  };
}

/**
 * Queues one score for the project's feedback route and returns at once; it never throws. Scores are sent in call
 * order, each after the spans that ended before it. Without a project id, or when 1,024 scores are already
 * waiting, the score is dropped and counted in `stats().feedbackDropped`. Before `init` and after `shutdown` it
 * does nothing.
 */
export function feedback(score: Feedback): void {
  state?.feedback.add(score);
}

/**
 * Exports every queued span and sends every queued score. Resolves within `exportTimeoutMs`, whether or not Crucible
 * answers.
 */
export async function flush(): Promise<void> {
  const current = state;
  await Promise.all([current?.provider.forceFlush(), current?.feedback.flush()]);
}

/**
 * Flushes, then stops exporting, within `exportTimeoutMs`. Spans that end and scores queued afterwards are dropped.
 */
export async function shutdown(): Promise<void> {
  const current = state;
  state = undefined;
  if (current === undefined) {
    return;
  }
  // The spans first, then the scores, within one `exportTimeoutMs` in all.
  const deadline = performance.now() + current.timeoutMs;
  await current.provider.shutdown();
  await current.feedback.shutdown(Math.max(0, deadline - performance.now()));
}

/** Counters since `init`; all zero before `init`. */
export function stats(): Stats {
  const queue = state?.queue.stats() ?? {
    queued: 0,
    queuedBytes: 0,
    dropped: 0,
    exported: 0,
    failed: 0,
  };
  const scores = state?.feedback.stats() ?? {
    feedbackSent: 0,
    feedbackDropped: 0,
    feedbackFailed: 0,
  };
  return { ...queue, ...scores };
}

/** The tracer for Crucible spans: the SDK's own after `init`, else the global one (a no-op without a provider). */
export function tracer(): Tracer {
  return state?.tracer ?? trace.getTracer(SDK_NAME, SDK_VERSION);
}

/** The secret key passed to `init`, which keys the multi-round state hashes (contract 9.6). */
export function secretKey(): string | undefined {
  return state?.secretKey;
}

function tracesUrl(endpoint: string): string {
  let url: URL;
  try {
    url = new URL(endpoint);
  } catch {
    throw new Error(
      `Crucible endpoint "${endpoint}" is not a URL: pass the endpoint option or set CRUCIBLE_ENDPOINT to the OTLP listener, for example http://localhost:4318.`,
    );
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new Error(
      `Crucible endpoint "${endpoint}" must use http or https: pass the endpoint option or set CRUCIBLE_ENDPOINT.`,
    );
  }
  if (!url.pathname.endsWith("/v1/traces")) {
    url.pathname = `${url.pathname.replace(/\/+$/, "")}/v1/traces`;
  }
  return url.href;
}

/** The project's feedback route, or `undefined` without a project id. */
function feedbackRoute(
  apiUrl: string | undefined,
  traces: string,
  projectId: string | undefined,
): string | undefined {
  let base: URL;
  if (apiUrl === undefined || apiUrl === "") {
    base = new URL(traces);
    base.pathname = base.pathname.replace(/\/v1\/traces$/, "");
  } else {
    try {
      base = new URL(apiUrl);
    } catch {
      throw new Error(
        `Crucible API URL "${apiUrl}" is not a URL: pass the apiUrl option or set CRUCIBLE_API_URL to the API listener, for example http://localhost:8080.`,
      );
    }
    if (base.protocol !== "http:" && base.protocol !== "https:") {
      throw new Error(
        `Crucible API URL "${apiUrl}" must use http or https: pass the apiUrl option or set CRUCIBLE_API_URL.`,
      );
    }
  }
  if (projectId === undefined || projectId === "") {
    return undefined;
  }
  if (!UUID.test(projectId)) {
    throw new Error(
      `Crucible init() option projectId "${projectId}" is not a project id: pass the project's UUID, not its slug, or set CRUCIBLE_PROJECT_ID; \`crucible admin create-key\` prints it as project_id.`,
    );
  }
  base.search = "";
  base.hash = "";
  return `${base.href.replace(/\/+$/, "")}/api/v1/projects/${projectId.toLowerCase()}/feedback`;
}

function positive(value: number | undefined, fallback: number, name: string): number {
  if (value === undefined) {
    return fallback;
  }
  if (!Number.isInteger(value) || value <= 0) {
    throw new Error(`Crucible init() option ${name} must be a positive integer; got ${value}.`);
  }
  return value;
}

function resourceAttributes(options: InitOptions): Attributes {
  const attributes: Attributes = { "service.instance.id": randomUUID() };
  const set = (key: string, value: string | undefined): void => {
    if (value !== undefined) {
      attributes[key] = value;
    }
  };
  set("service.name", options.serviceName);
  set("service.version", options.serviceVersion);
  set("deployment.environment.name", options.environment);
  set("crucible.deploy.id", options.deployId);
  return attributes;
}
