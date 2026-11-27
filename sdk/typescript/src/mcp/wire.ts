// Helpers shared by the MCP server tap, the HTTP edge companion and the MCP client wrapper. Nothing here
// imports a runtime value from the MCP SDK, so the client wrapper works without the server package.

import { createHash, createHmac } from "node:crypto";
import { type Attributes, type Span, SpanStatusCode, type TextMapGetter } from "@opentelemetry/api";
import { W3CTraceContextPropagator } from "@opentelemetry/core";
import { cutUtf8, scrub } from "../scrub.ts";
import type { Annotations } from "../scope.ts";

export type Era = "legacy" | "modern";

/** Contract 1.6 `Transport`, sent as `crucible.mcp.transport`. */
export type TransportLabel = "stdio" | "streamable_http" | "sse" | "unknown";

export type Fields = Record<string, unknown>;

/** Reserved `_meta` keys of the per-request envelope (protocol revision 2026-07-28). */
export const PROTOCOL_VERSION_KEY = "io.modelcontextprotocol/protocolVersion";
export const CLIENT_INFO_KEY = "io.modelcontextprotocol/clientInfo";
export const SERVER_INFO_KEY = "io.modelcontextprotocol/serverInfo";

/** W3C trace context in `params._meta` (contract 9.1). Baggage is never read or written. */
export const propagator = new W3CTraceContextPropagator();

export const metaGetter: TextMapGetter<Fields> = {
  keys: (carrier) => Object.keys(carrier),
  get: (carrier, key) => text(carrier[key]),
};

export function fields(value: unknown): Fields | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Fields)
    : undefined;
}

export function text(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

/** A name and version pair such as `clientInfo` or `serverInfo`. */
export interface Implementation {
  name?: string;
  version?: string;
}

export function implementation(value: unknown): Implementation | undefined {
  const info = fields(value);
  if (info === undefined) {
    return undefined;
  }
  const name = text(info.name);
  const version = text(info.version);
  return {
    ...(name !== undefined && { name }),
    ...(version !== undefined && { version }),
  };
}

/** Span attributes that name the target of a request (contract 9.1). */
export function targetAttributes(method: string, params: Fields | undefined): Attributes {
  const attributes: Attributes = {};
  if (method === "tools/call") {
    put(attributes, "gen_ai.tool.name", text(params?.name));
  } else if (method === "prompts/get") {
    put(attributes, "gen_ai.prompt.name", text(params?.name));
  } else if (method.startsWith("resources/")) {
    put(attributes, "crucible.mcp.resource.template", text(params?.uri));
  }
  return attributes;
}

export function spanName(method: string, params: Fields | undefined): string {
  const name = method === "tools/call" || method === "prompts/get" ? text(params?.name) : undefined;
  return name === undefined ? method : `${method} ${name}`;
}

export function put(attributes: Attributes, key: string, value: string | undefined): void {
  if (value !== undefined) {
    attributes[key] = value;
  }
}

export function putImplementation(
  attributes: Attributes,
  prefix: string,
  info: Implementation | undefined,
): void {
  put(attributes, `${prefix}.name`, info?.name);
  put(attributes, `${prefix}.version`, info?.version);
}

/** The text of the first text block of a tool result. */
export function firstText(result: Fields): string | undefined {
  if (!Array.isArray(result.content)) {
    return undefined;
  }
  for (const block of result.content) {
    const item = fields(block);
    if (item?.type === "text") {
      return text(item.text);
    }
  }
  return undefined;
}

/** Error message cap (contract 3.5 `error_message`). */
const ERROR_MESSAGE_BYTES = 2_048;

/** Room past the cap that scrubbing sees, so a secret that straddles the cap is still matched whole. */
const SCRUB_MARGIN = 1_024;

/**
 * An error message as Crucible stores it (contract 3.5 `error_message`): scrubbed with the shared rules, then cut
 * at 2 KiB on a character boundary. Only the part the cut can keep, and the margin after it, is scrubbed.
 */
function errorMessage(message: string): string {
  return cutUtf8(scrub(message.slice(0, ERROR_MESSAGE_BYTES + SCRUB_MARGIN)), ERROR_MESSAGE_BYTES);
}

/**
 * Records the evidence a JSON-RPC response carries for the error taxonomy (contract 9.2): the code of an error
 * response, `tool_error` for a result with `isError`, and the SDK annotations that refine them. Crucible's
 * classifier derives the class from this evidence. The status message is the error's message, or the text of the
 * first text block of a result with `isError`, which holds a thrown error's message. Returns the result of a
 * result response.
 */
export function applyResponse(
  span: Span,
  response: { error?: unknown; result?: unknown },
  annotations: Annotations | undefined,
): Fields | undefined {
  const error = fields(response.error);
  if (error !== undefined) {
    const code = typeof error.code === "number" ? error.code : undefined;
    if (code !== undefined) {
      span.setAttributes({ "rpc.response.status_code": String(code), "error.type": String(code) });
      if (code === -32602 && annotations?.notFound === true) {
        span.setAttribute("crucible.mcp.error.class", "not_found");
      }
    }
    span.setStatus({
      code: SpanStatusCode.ERROR,
      message: errorMessage(text(error.message) ?? ""),
    });
    return undefined;
  }
  const result = fields(response.result) ?? {};
  if (result.isError === true) {
    span.setAttribute("error.type", "tool_error");
    if (annotations?.validation === true) {
      span.setAttribute("crucible.mcp.error.class", "validation");
    } else if (annotations?.notFound === true) {
      span.setAttribute("crucible.mcp.error.class", "not_found");
    }
    if (annotations?.subclass !== undefined) {
      span.setAttribute("crucible.mcp.error.subclass", annotations.subclass);
    }
    span.setStatus({ code: SpanStatusCode.ERROR, message: errorMessage(firstText(result) ?? "") });
  }
  span.setAttribute(
    "crucible.mcp.result_type",
    result.resultType === "input_required" ? "input_required" : "complete",
  );
  return result;
}

/** Evidence for an operation still pending when its connection closed (contract 9.2 rows 1 and 3). */
export function applyClosed(span: Span, streamPerRequest: boolean): void {
  if (streamPerRequest) {
    // In the 2026-07-28 HTTP binding, closing a request's own stream is how a client cancels it.
    span.setAttribute("crucible.mcp.cancel.source", "stream_close");
    return;
  }
  span.setAttributes({ "error.type": "transport", "crucible.mcp.error.class": "transport" });
  span.setStatus({
    code: SpanStatusCode.ERROR,
    message: "The connection closed before the request was answered.",
  });
}

/** `mcp.session.id` (contract 9.4): the first 16 bytes of SHA-256 of the raw session id, as hex. */
export function sessionKey(sessionId: string): string {
  return createHash("sha256").update(sessionId).digest("hex").slice(0, 32);
}

/** `crucible.mcp.mrtr.state_*` (contract 9.6): a keyed hash of a `requestState`, never the state itself. */
export function stateHash(secretKey: string, requestState: string): string {
  return createHmac("sha256", secretKey)
    .update("crucible.mrtr.v1")
    .update(new Uint8Array([0]))
    .update(requestState)
    .digest("hex")
    .slice(0, 32);
}

type Callback = (...args: never[]) => unknown;

/**
 * Routes assignments of a transport callback through `wrap`. The MCP SDK assigns the callbacks during
 * `connect`, after the tap, so no message is dispatched around it. Some transports keep the callback in a plain
 * field; others forward it to an inner transport through an accessor on their class. An existing accessor stays
 * in the path, so the forwarding still happens.
 */
export function interpose<T extends object, K extends keyof T & string>(
  target: T,
  key: K,
  wrap: (next: NonNullable<T[K]> & Callback) => T[K],
): void {
  const inherited = findAccessor(target, key);
  let current: T[K] | undefined = inherited === undefined ? target[key] : undefined;
  Object.defineProperty(target, key, {
    configurable: true,
    enumerable: true,
    get: (): T[K] | undefined =>
      inherited?.get === undefined ? current : inherited.get.call(target),
    set: (next: T[K] | undefined) => {
      const wrapped =
        typeof next === "function" ? wrap(next as NonNullable<T[K]> & Callback) : undefined;
      if (inherited?.set === undefined) {
        current = wrapped;
      } else {
        inherited.set.call(target, wrapped);
      }
    },
  });
}

function findAccessor(target: object, key: string): PropertyDescriptor | undefined {
  for (
    let holder: object | null = target;
    holder !== null;
    holder = Object.getPrototypeOf(holder)
  ) {
    const descriptor = Object.getOwnPropertyDescriptor(holder, key);
    if (descriptor !== undefined) {
      return descriptor.get === undefined && descriptor.set === undefined ? undefined : descriptor;
    }
  }
  return undefined;
}

/** Whether the tap can be installed without throwing: the object is extensible and the members are replaceable. */
export function tappable(target: object, callbacks: readonly string[], method: string): boolean {
  if (!Object.isExtensible(target)) {
    return false;
  }
  for (const key of callbacks) {
    if (Object.getOwnPropertyDescriptor(target, key)?.configurable === false) {
      return false;
    }
  }
  const own = Object.getOwnPropertyDescriptor(target, method);
  return own === undefined || own.writable === true || own.configurable === true;
}

/** Runs telemetry work so that a failure in it never reaches the MCP traffic. */
export function safely(work: () => void): void {
  try {
    work();
  } catch {
    // Telemetry never affects the wire.
  }
}
