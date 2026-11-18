// The SDK's own async context. It carries the innermost Crucible span, the values set by `withContext`, and the
// annotations of the MCP server operation being handled. It does not depend on an OpenTelemetry context manager
// being registered; when the host has one, spans also nest under and over the host's spans.

import { AsyncLocalStorage } from "node:async_hooks";
import { type Attributes, type Context, context, type Span, trace } from "@opentelemetry/api";

/** Values that every span started inside `withContext` carries (contract 10.6 keys). */
export interface TraceContext {
  /** `crucible.session.id`. */
  sessionId?: string;
  /** `crucible.user.id`. */
  userId?: string;
  /** `crucible.tags`; nested scopes add to the outer tags. */
  tags?: string[];
  /** `crucible.metadata.<key>`; nested scopes override outer keys. */
  metadata?: Record<string, string | number | boolean>;
}

/** An error subclass (contract 9.2); it refines a tool error. */
export type ErrorSubclass = "auth" | "rate_limit" | "upstream" | "business";

/** Identity of the caller of an MCP operation (contract 9.3 and 9.8). */
export interface Identity {
  /** `user.id`. */
  userId?: string;
  /** `crucible.mcp.tenant`. */
  tenant?: string;
  /** `crucible.mcp.user.issuer`: the token issuer, sent raw and kept for delete-by-user. */
  issuer?: string;
  /** `crucible.mcp.user.subject`: sent raw; Crucible hashes it at ingest and discards it. */
  subject?: string;
  /** `crucible.mcp.handle.id` (contract 9.8). */
  handleId?: string;
}

/** What the handler of one MCP server operation reported through `annotateError`, `mark*` and `identify`. */
export interface Annotations {
  subclass?: ErrorSubclass;
  validation?: boolean;
  notFound?: boolean;
  identity?: Identity;
}

export interface Scope {
  readonly span?: Span;
  readonly values?: TraceContext;
  readonly annotations?: Annotations;
}

const store = new AsyncLocalStorage<Scope>();

export function currentScope(): Scope | undefined {
  return store.getStore();
}

/** Runs `fn` in `scope`. A span in the scope also becomes the active span of the host's context, if any. */
export function runInScope<A extends unknown[], R>(
  scope: Scope,
  fn: (...args: A) => R,
  ...args: A
): R {
  const span = scope.span;
  if (span === undefined) {
    return store.run(scope, fn, ...args);
  }
  return store.run(scope, () =>
    context.with(trace.setSpan(context.active(), span), fn, undefined, ...args),
  );
}

/** The parent for a new span: the innermost Crucible span, else the host's active span, else none. */
export function parentContext(): Context {
  const span = store.getStore()?.span;
  return span === undefined ? context.active() : trace.setSpan(context.active(), span);
}

/** The span attributes for the current `withContext` values. */
export function contextAttributes(): Attributes {
  const values = store.getStore()?.values;
  const attributes: Attributes = {};
  if (values === undefined) {
    return attributes;
  }
  if (values.sessionId !== undefined) {
    attributes["crucible.session.id"] = values.sessionId;
  }
  if (values.userId !== undefined) {
    attributes["crucible.user.id"] = values.userId;
  }
  if (values.tags !== undefined && values.tags.length > 0) {
    attributes["crucible.tags"] = values.tags;
  }
  for (const [key, value] of Object.entries(values.metadata ?? {})) {
    attributes[`crucible.metadata.${key}`] = value;
  }
  return attributes;
}

/** Merges `inner` into `outer`: scalars and metadata keys of `inner` win, tags are added. */
export function mergeValues(outer: TraceContext | undefined, inner: TraceContext): TraceContext {
  const merged: TraceContext = { ...outer, ...inner };
  if (outer?.tags !== undefined || inner.tags !== undefined) {
    merged.tags = [...new Set([...(outer?.tags ?? []), ...(inner.tags ?? [])])];
  }
  if (outer?.metadata !== undefined || inner.metadata !== undefined) {
    merged.metadata = { ...outer?.metadata, ...inner.metadata };
  }
  return merged;
}
