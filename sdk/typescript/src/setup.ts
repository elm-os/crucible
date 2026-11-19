// SDK setup: one private tracer provider that exports OTLP/HTTP protobuf to Crucible's `/v1/traces`.
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
import { QueueSpanProcessor, type QueueStats } from "./queue.ts";

export const SDK_NAME = "@elm-os/crucible";
export const SDK_VERSION = "0.0.0";

export interface InitOptions {
  /** Base URL of a Crucible OTLP listener. Default: `CRUCIBLE_ENDPOINT`, else `http://localhost:4318`. */
  endpoint?: string;
  /** Project secret key with the `ingest` scope, sent as a Bearer token. Default: `CRUCIBLE_SECRET_KEY`. */
  secretKey?: string;
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
  secretKey: string;
}

let state: State | undefined;

/** Starts exporting to Crucible. Throws on a missing key or a malformed endpoint, naming the setting to fix. */
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
  const timeoutMs = positive(options.exportTimeoutMs, 10_000, "exportTimeoutMs");
  const queue = new QueueSpanProcessor(
    new OTLPTraceExporter({
      url,
      headers: { Authorization: `Bearer ${key}` },
      timeoutMillis: timeoutMs,
      userAgent: `crucible-sdk-typescript/${SDK_VERSION}`,
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
  state = { provider, tracer: provider.getTracer(SDK_NAME, SDK_VERSION), queue, secretKey: key };
}

/** Exports every queued span. Resolves within `exportTimeoutMs`, whether or not the collector answers. */
export async function flush(): Promise<void> {
  await state?.provider.forceFlush();
}

/** Flushes, then stops exporting. Spans that end afterwards are dropped. */
export async function shutdown(): Promise<void> {
  const current = state;
  state = undefined;
  await current?.provider.shutdown();
}

/** Export queue counters since `init`; all zero before `init`. */
export function stats(): QueueStats {
  return state?.queue.stats() ?? { queued: 0, queuedBytes: 0, dropped: 0, exported: 0, failed: 0 };
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
