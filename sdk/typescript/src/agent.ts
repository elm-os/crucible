// Agent helpers: `withContext` for session, user, tags and metadata, and `observe` for typed spans with
// generation usage, provided cost and a prompt reference (contract 10.6 keys).

import { type Attributes, type Span, SpanKind, SpanStatusCode } from "@opentelemetry/api";
import {
  contextAttributes,
  currentScope,
  mergeValues,
  parentContext,
  runInScope,
  type TraceContext,
} from "./scope.ts";
import { tracer } from "./setup.ts";

/** Observation types (contract 1.6 `ObservationType`), sent as `crucible.observation.type`. */
export type ObservationType =
  | "span"
  | "generation"
  | "event"
  | "embedding"
  | "agent"
  | "tool"
  | "chain"
  | "retriever"
  | "evaluator"
  | "guardrail";

export interface ObserveOptions {
  /** Without a type, Crucible infers one from the span's attributes, else `span`. */
  type?: ObservationType;
  /** `crucible.input`: a string as is, anything else as JSON. */
  input?: unknown;
}

/** Token counts of a generation. Cache and reasoning counts are parts of input and output, not additions. */
export interface Usage {
  input?: number;
  output?: number;
  cacheRead?: number;
  cacheWrite?: number;
  reasoning?: number;
}

/** A prompt reference (contract 3.1 `prompt_name`, `prompt_version`). */
export interface PromptRef {
  name: string;
  version?: string | number;
}

export interface ObservationUpdate {
  /** `crucible.input`: a string as is, anything else as JSON. */
  input?: unknown;
  /** `crucible.output`: a string as is, anything else as JSON. */
  output?: unknown;
  /** `gen_ai.request.model`. */
  model?: string;
  /** `gen_ai.response.model`. */
  responseModel?: string;
  /** `gen_ai.provider.name`. */
  provider?: string;
  usage?: Usage;
  /**
   * Provided cost in USD per usage key (`total`, `input`, `output`, `cache_read`, `cache_write`, `reasoning`),
   * sent as `crucible.cost.<key>`. A provided cost wins over the price table. Strings keep decimal precision.
   */
  cost?: Record<string, number | string>;
  prompt?: PromptRef;
}

/** The handle `observe` passes to its callback. */
export interface Observation {
  /** The underlying OpenTelemetry span. */
  readonly span: Span;
  /** Sets input, output, model, usage, cost or prompt fields on the span. */
  update(fields: ObservationUpdate): void;
}

const USAGE_KEYS: Record<keyof Usage, string> = {
  input: "gen_ai.usage.input_tokens",
  output: "gen_ai.usage.output_tokens",
  cacheRead: "gen_ai.usage.cache_read.input_tokens",
  cacheWrite: "gen_ai.usage.cache_creation.input_tokens",
  reasoning: "gen_ai.usage.reasoning.output_tokens",
};

/** Runs `fn` with session, user, tags and metadata that every span started inside it carries. */
export function withContext<T>(values: TraceContext, fn: () => T): T {
  const scope = currentScope();
  return runInScope({ ...scope, values: mergeValues(scope?.values, values) }, fn);
}

/**
 * Runs `fn` inside a new span. The span ends when `fn` returns, or when the promise it returns settles; a
 * throw or a rejection marks the span as an error and is passed on unchanged.
 */
export function observe<T>(name: string, fn: (observation: Observation) => T): T;
export function observe<T>(
  name: string,
  options: ObserveOptions,
  fn: (observation: Observation) => T,
): T;
export function observe<T>(
  name: string,
  optionsOrFn: ObserveOptions | ((observation: Observation) => T),
  maybeFn?: (observation: Observation) => T,
): T {
  const options = typeof optionsOrFn === "function" ? {} : optionsOrFn;
  const fn = typeof optionsOrFn === "function" ? optionsOrFn : maybeFn;
  if (fn === undefined) {
    throw new TypeError("observe(name, options, fn) needs a callback as its last argument.");
  }
  const attributes = contextAttributes();
  if (options.type !== undefined) {
    attributes["crucible.observation.type"] = options.type;
  }
  if (options.input !== undefined) {
    attributes["crucible.input"] = body(options.input);
  }
  const span = tracer().startSpan(name, { kind: SpanKind.INTERNAL, attributes }, parentContext());
  const observation: Observation = {
    span,
    update: (fields) => span.setAttributes(updateAttributes(fields)),
  };
  const fail = (error: unknown): void => {
    span.recordException(error instanceof Error ? error : String(error));
    span.setStatus({
      code: SpanStatusCode.ERROR,
      message: error instanceof Error ? error.message : String(error),
    });
    span.end();
  };
  let result: T;
  try {
    result = runInScope({ ...currentScope(), span }, fn, observation);
  } catch (error) {
    fail(error);
    throw error;
  }
  if (result instanceof Promise) {
    return result.then(
      (value: unknown) => {
        span.end();
        return value;
      },
      (error: unknown) => {
        fail(error);
        throw error;
      },
    ) as T;
  }
  span.end();
  return result;
}

function updateAttributes(fields: ObservationUpdate): Attributes {
  const attributes: Attributes = {};
  if (fields.input !== undefined) {
    attributes["crucible.input"] = body(fields.input);
  }
  if (fields.output !== undefined) {
    attributes["crucible.output"] = body(fields.output);
  }
  if (fields.model !== undefined) {
    attributes["gen_ai.request.model"] = fields.model;
  }
  if (fields.responseModel !== undefined) {
    attributes["gen_ai.response.model"] = fields.responseModel;
  }
  if (fields.provider !== undefined) {
    attributes["gen_ai.provider.name"] = fields.provider;
  }
  for (const [name, key] of Object.entries(USAGE_KEYS)) {
    const count = fields.usage?.[name as keyof Usage];
    if (count !== undefined) {
      attributes[key] = count;
    }
  }
  for (const [key, usd] of Object.entries(fields.cost ?? {})) {
    attributes[`crucible.cost.${key}`] = usd;
  }
  if (fields.prompt !== undefined) {
    attributes["crucible.prompt.name"] = fields.prompt.name;
    if (fields.prompt.version !== undefined) {
      attributes["crucible.prompt.version"] = String(fields.prompt.version);
    }
  }
  return attributes;
}

function body(value: unknown): string {
  return typeof value === "string" ? value : JSON.stringify(value);
}
