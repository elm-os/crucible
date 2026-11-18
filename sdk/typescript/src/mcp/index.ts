// `@elm-os/crucible/mcp`: instrumentation for MCP servers built on `@modelcontextprotocol/server` 2.x.

import { currentScope, type ErrorSubclass, type Identity } from "../scope.ts";

export type { ErrorSubclass, Identity } from "../scope.ts";
export type { CaptureRule } from "./capture.ts";
export { type FetchHandler, type IdentityOptions, instrumentHandler } from "./edge.ts";
export { type InstrumentOptions, instrument } from "./server.ts";
export type { TransportLabel } from "./wire.ts";

/**
 * Refines a tool error of the operation being handled (contract 9.2): `auth`, `rate_limit`, `upstream` or
 * `business`. No-op outside an MCP handler.
 */
export function annotateError(subclass: ErrorSubclass): void {
  const annotations = currentScope()?.annotations;
  if (annotations !== undefined) {
    annotations.subclass = subclass;
  }
}

/** Marks the operation's tool error as a validation failure. No-op outside an MCP handler. */
export function markValidation(): void {
  const annotations = currentScope()?.annotations;
  if (annotations !== undefined) {
    annotations.validation = true;
  }
}

/**
 * Marks the operation's error as "not found": a tool error, or an invalid-params error (-32602). No-op outside
 * an MCP handler.
 */
export function markNotFound(): void {
  const annotations = currentScope()?.annotations;
  if (annotations !== undefined) {
    annotations.notFound = true;
  }
}

/** Merges identity fields into the operation being handled (contract 9.3). No-op outside an MCP handler. */
export function identify(identity: Identity): void {
  const annotations = currentScope()?.annotations;
  if (annotations !== undefined) {
    annotations.identity = { ...annotations.identity, ...identity };
  }
}
