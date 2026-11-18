// The HTTP edge companion. `createMcpHandler` answers some exchanges before any server instance is connected
// (a wrong content type, an unsupported revision, `subscriptions/listen` and the other early returns), so the
// transport tap never sees them. `instrumentHandler` wraps the handler's `fetch` and records an operation only
// for an exchange that the tap did not see.

import { AsyncLocalStorage } from "node:async_hooks";
import { performance } from "node:perf_hooks";
import type {
  AuthInfo,
  McpHandlerRequestOptions,
  MessageExtraInfo,
} from "@modelcontextprotocol/server";
import { type Attributes, ROOT_CONTEXT, SpanKind, SpanStatusCode } from "@opentelemetry/api";
import { cutUtf8, scrub } from "../scrub.ts";
import type { Identity } from "../scope.ts";
import { tracer } from "../setup.ts";
import {
  CLIENT_INFO_KEY,
  fields,
  implementation,
  metaGetter,
  PROTOCOL_VERSION_KEY,
  propagator,
  put,
  putImplementation,
  spanName,
  targetAttributes,
  text,
} from "./wire.ts";

/** Identity options shared by `instrument` and `instrumentHandler` (contract 9.3). */
export interface IdentityOptions {
  /**
   * Called once per inbound message, before dispatch, with the transport's `authInfo` and HTTP request.
   * Returns the caller's identity; `identify()` inside a handler adds to it.
   */
  identify?: (extra: MessageExtraInfo | undefined) => Identity | undefined;
  /** `"none"` suppresses every identity key, including the OAuth client id. */
  identity?: "none";
}

/**
 * The attributes of the caller's identity. `identity_source` names where it came from. `argumentHandle` is the
 * handle id read from a tool argument (`handleParams`); a handle id from `identify` wins over it.
 */
export function identityAttributes(
  options: IdentityOptions,
  identity: Identity | undefined,
  authInfo: AuthInfo | undefined,
  argumentHandle?: string,
): Attributes {
  const attributes: Attributes = {};
  if (options.identity === "none") {
    return attributes;
  }
  put(attributes, "user.id", identity?.userId);
  put(attributes, "crucible.mcp.tenant", identity?.tenant);
  put(attributes, "crucible.mcp.user.issuer", identity?.issuer);
  put(attributes, "crucible.mcp.user.subject", identity?.subject);
  put(attributes, "crucible.mcp.handle.id", handleId(identity?.handleId));
  const hooked = Object.keys(attributes).length > 0;
  put(
    attributes,
    "crucible.mcp.handle.id",
    text(attributes["crucible.mcp.handle.id"]) ?? argumentHandle,
  );
  put(attributes, "crucible.mcp.oauth.client_id", text(authInfo?.clientId));
  if (hooked) {
    attributes["crucible.mcp.identity.source"] = "identify_hook";
  } else if (authInfo !== undefined) {
    // The hosting server verified the token that produced `authInfo`.
    attributes["crucible.mcp.identity.source"] = "token_verified";
  }
  return attributes;
}

/** A handle id (contract 9.8): scrubbed, then cut at 256 bytes. */
export function handleId(value: string | undefined): string | undefined {
  return value === undefined ? undefined : cutUtf8(scrub(value), 256);
}

/** Calls the `identify` hook, ignoring a hook that throws. */
export function hookIdentity(
  options: IdentityOptions,
  extra: MessageExtraInfo | undefined,
): Identity | undefined {
  if (options.identity === "none" || options.identify === undefined) {
    return undefined;
  }
  try {
    const identity = options.identify(extra);
    return identity === undefined ? undefined : { ...identity };
  } catch {
    return undefined;
  }
}

interface EdgeScope {
  seen: number;
}

const edgeScope = new AsyncLocalStorage<EdgeScope>();

/** Called by the transport tap for each message it sees, so the edge companion does not count it again. */
export function noteConnectSeen(): void {
  const scope = edgeScope.getStore();
  if (scope !== undefined) {
    scope.seen += 1;
  }
}

/** The fetch face of a `createMcpHandler` result. */
export interface FetchHandler {
  fetch: (request: Request, options?: McpHandlerRequestOptions) => Promise<Response>;
}

/**
 * Returns a copy of `handler` whose `fetch` also records the exchanges that end before a server instance is
 * connected. Use it together with `instrument` on the factory: `instrumentHandler(createMcpHandler(instrument(f)))`.
 */
export function instrumentHandler<H extends FetchHandler>(
  handler: H,
  options: IdentityOptions = {},
): H {
  const inner = handler.fetch.bind(handler);
  const fetch: FetchHandler["fetch"] = async (request, requestOptions) => {
    const scope: EdgeScope = { seen: 0 };
    const startTime = performance.timeOrigin + performance.now();
    const probe =
      request.method === "POST" && requestOptions?.parsedBody === undefined
        ? request.clone()
        : undefined;
    const response = await edgeScope.run(scope, inner, request, requestOptions);
    if (scope.seen > 0) {
      void probe?.body?.cancel().catch(() => {});
      return response;
    }
    const isJson = (response.headers.get("content-type") ?? "").includes("application/json");
    void recordEdge({
      request,
      parsedBody: requestOptions?.parsedBody,
      authInfo: requestOptions?.authInfo,
      probe,
      status: response.status,
      responseProbe: isJson ? response.clone() : undefined,
      startTime,
      options,
    }).catch(() => {});
    return response;
  };
  return { ...handler, fetch };
}

interface EdgeExchange {
  request: Request;
  parsedBody: unknown;
  authInfo: AuthInfo | undefined;
  probe: Request | undefined;
  status: number;
  responseProbe: Response | undefined;
  startTime: number;
  options: IdentityOptions;
}

async function recordEdge(exchange: EdgeExchange): Promise<void> {
  const endTime = performance.timeOrigin + performance.now();
  const body = fields(exchange.parsedBody ?? (await readJson(exchange.probe)));
  const method = text(body?.method);
  if (method === undefined) {
    // No JSON-RPC method, so no MCP operation (for example a refused GET of a legacy event stream).
    return;
  }
  const params = fields(body?.params);
  const meta = fields(params?._meta);
  const claimed = text(meta?.[PROTOCOL_VERSION_KEY]);
  const id = body?.id;
  const isRequest = typeof id === "string" || typeof id === "number";
  const attributes: Attributes = {
    "mcp.method.name": method,
    "crucible.mcp.capture_source": "sdk",
    "crucible.mcp.era": claimed === undefined ? "legacy" : "modern",
    "crucible.mcp.transport": "streamable_http",
    "http.response.status_code": exchange.status,
    ...targetAttributes(method, params),
  };
  if (isRequest) {
    attributes["jsonrpc.request.id"] = String(id);
    if (method === "subscriptions/listen") {
      attributes["crucible.mcp.subscription.id"] = String(id);
    }
  } else {
    attributes["crucible.mcp.is_notification"] = true;
  }
  put(
    attributes,
    "mcp.protocol.version",
    claimed ?? exchange.request.headers.get("mcp-protocol-version") ?? undefined,
  );
  putImplementation(attributes, "crucible.mcp.client", implementation(meta?.[CLIENT_INFO_KEY]));
  put(attributes, "user_agent.original", exchange.request.headers.get("user-agent") ?? undefined);
  const extra: MessageExtraInfo = {
    request: exchange.request,
    ...(exchange.authInfo !== undefined && { authInfo: exchange.authInfo }),
  };
  Object.assign(
    attributes,
    identityAttributes(exchange.options, hookIdentity(exchange.options, extra), exchange.authInfo),
  );
  const parent = propagator.extract(ROOT_CONTEXT, meta ?? {}, metaGetter);
  const span = tracer().startSpan(
    spanName(method, params),
    { kind: SpanKind.SERVER, attributes, startTime: exchange.startTime },
    parent,
  );
  const error = fields(fields(await readJson(exchange.responseProbe))?.error);
  const code = typeof error?.code === "number" ? error.code : undefined;
  if (code !== undefined) {
    span.setAttributes({ "rpc.response.status_code": String(code), "error.type": String(code) });
  } else if (exchange.status >= 400) {
    span.setAttribute("error.type", String(exchange.status));
  }
  if (code !== undefined || exchange.status >= 400) {
    span.setStatus({
      code: SpanStatusCode.ERROR,
      message: cutUtf8(scrub(text(error?.message) ?? ""), 2_048),
    });
  }
  span.end(endTime);
}

async function readJson(source: Request | Response | undefined): Promise<unknown> {
  if (source === undefined) {
    return undefined;
  }
  try {
    return JSON.parse(await source.text());
  } catch {
    return undefined;
  }
}
