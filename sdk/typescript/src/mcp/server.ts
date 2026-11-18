// MCP server instrumentation (docs/architecture/mcp-typescript-seam.md). `instrument` replaces the public `connect`
// of a server instance (and of an `McpServer`'s inner server), and taps three public members of the transport it
// is given, in place: `onmessage`, `send` and `onclose`. Each inbound request or notification becomes one SERVER
// span with the contract 9.1 keys. No private member of the SDK is read.

import type {
  JSONRPCMessage,
  JSONRPCNotification,
  JSONRPCRequest,
  McpRequestContext,
  McpServer,
  MessageExtraInfo,
  Server,
  Transport,
} from "@modelcontextprotocol/server";
import { StdioServerTransport } from "@modelcontextprotocol/server/stdio";
import { type Attributes, ROOT_CONTEXT, type Span, SpanKind } from "@opentelemetry/api";
import { type Annotations, runInScope } from "../scope.ts";
import { secretKey, tracer } from "../setup.ts";
import { type CaptureDecision, type CaptureRule, captured, decide } from "./capture.ts";
import {
  handleId,
  hookIdentity,
  type IdentityOptions,
  identityAttributes,
  noteConnectSeen,
} from "./edge.ts";
import {
  applyClosed,
  applyResponse,
  CLIENT_INFO_KEY,
  type Era,
  type Fields,
  fields,
  type Implementation,
  implementation,
  interpose,
  metaGetter,
  PROTOCOL_VERSION_KEY,
  propagator,
  put,
  putImplementation,
  SERVER_INFO_KEY,
  safely,
  sessionKey,
  spanName,
  stateHash,
  type TransportLabel,
  tappable,
  targetAttributes,
  text,
} from "./wire.ts";

export interface InstrumentOptions extends IdentityOptions {
  /** Transport label override. By default HTTP messages are `streamable_http`, stdio is `stdio`. */
  transport?: TransportLabel;
  /** Content capture rules by tool glob; content is not captured without a matching rule. */
  capture?: CaptureRule[];
  /** Per tool, the argument whose string value is the operation's handle id (contract 9.8). */
  handleParams?: Record<string, string>;
}

type Product = McpServer | Server;
type Factory = (ctx: McpRequestContext) => Product | Promise<Product>;
type Dispatch = NonNullable<Transport["onmessage"]>;
type RequestId = string | number;

/** What all instances built by one `instrument` call share: they are one server. */
interface Shared {
  /** The server's name and version, from the first `initialize` result or result `_meta` that carries them. */
  server: Implementation | undefined;
}

interface Settings {
  readonly options: InstrumentOptions;
  readonly shared: Shared;
  /** Era hint from the factory context. The per-message classification and envelope win. */
  readonly era: Era | undefined;
  /** Transport hint from the factory context. */
  readonly transport: TransportLabel | undefined;
}

interface Connection {
  readonly transport: Transport;
  readonly settings: Settings;
  readonly label: TransportLabel;
  readonly pending: Map<RequestId, Operation>;
  client: Implementation | undefined;
  protocolVersion: string | undefined;
}

interface Operation {
  readonly span: Span;
  readonly method: string;
  readonly id: RequestId | undefined;
  readonly era: Era;
  readonly http: boolean;
  readonly authInfo: MessageExtraInfo["authInfo"];
  readonly annotations: Annotations;
  readonly handle: string | undefined;
  readonly capture: CaptureDecision | undefined;
  cut: boolean;
  progress: number;
}

const instrumented = new WeakSet<object>();
const tapped = new WeakSet<Transport>();

/**
 * Instruments an MCP server instance and returns it, or wraps a factory for `createMcpHandler` and `serveStdio`.
 * A transport that cannot be tapped is left uninstrumented; `connect` keeps working.
 */
export function instrument<S extends Product>(server: S, options?: InstrumentOptions): S;
export function instrument(factory: Factory, options?: InstrumentOptions): Factory;
export function instrument(
  target: Product | Factory,
  options: InstrumentOptions = {},
): Product | Factory {
  const shared: Shared = { server: undefined };
  if (typeof target === "function") {
    // A factory builds a fresh instance per HTTP request or stdio connection; they all share `shared`.
    return async (ctx: McpRequestContext) =>
      decorate(await target(ctx), {
        options,
        shared,
        era: ctx.era,
        // `createMcpHandler` always passes the HTTP request; `serveStdio` never does.
        transport: ctx.requestInfo === undefined ? "stdio" : "streamable_http",
      });
  }
  return decorate(target, { options, shared, era: undefined, transport: undefined });
}

function decorate<S extends Product>(product: S, settings: Settings): S {
  // `McpServer.connect` delegates to its inner server; user code may also connect the inner server directly.
  const targets: Product[] = "server" in product ? [product, product.server] : [product];
  for (const target of targets) {
    if (instrumented.has(target)) {
      continue;
    }
    try {
      const connect = target.connect.bind(target);
      target.connect = (transport: Transport): Promise<void> => {
        safely(() => tap(transport, settings));
        return connect(transport);
      };
      instrumented.add(target);
    } catch {
      // A frozen instance stays uninstrumented.
    }
  }
  return product;
}

function tap(transport: Transport, settings: Settings): void {
  if (tapped.has(transport) || !tappable(transport, ["onmessage", "onclose"], "send")) {
    return;
  }
  tapped.add(transport);
  const connection: Connection = {
    transport,
    settings,
    label: settings.transport ?? (transport instanceof StdioServerTransport ? "stdio" : "unknown"),
    pending: new Map(),
    client: undefined,
    protocolVersion: undefined,
  };
  interpose(transport, "onmessage", (next) => dispatch(next, connection));
  interpose(transport, "onclose", (next) => () => {
    safely(() => abandon(connection));
    next();
  });
  const send = transport.send.bind(transport);
  transport.send = (message, sendOptions) => {
    safely(() => outbound(message, sendOptions?.relatedRequestId, connection));
    return send(message, sendOptions);
  };
}

function dispatch(next: Dispatch, connection: Connection): Dispatch {
  return (message, extra) => {
    if (!("method" in message)) {
      // A client's answer to a request this server sent: not an operation of this server.
      next(message, extra);
      return;
    }
    let operation: Operation | undefined;
    safely(() => {
      if (message.method === "notifications/cancelled") {
        cancel(fields(message.params)?.requestId, connection);
      }
      operation = begin(message, extra, connection);
      noteConnectSeen();
    });
    if (operation === undefined) {
      next(message, extra);
      return;
    }
    const scope = { span: operation.span, annotations: operation.annotations };
    if (operation.id !== undefined) {
      connection.pending.set(operation.id, operation);
      runInScope(scope, () => next(message, extra));
      return;
    }
    try {
      runInScope(scope, () => next(message, extra));
    } finally {
      const done = operation;
      safely(() => finish(done, connection));
    }
  };
}

function begin(
  message: JSONRPCRequest | JSONRPCNotification,
  extra: MessageExtraInfo | undefined,
  connection: Connection,
): Operation {
  const { options } = connection.settings;
  const params = fields(message.params);
  const meta = fields(params?._meta);
  const claimed = text(meta?.[PROTOCOL_VERSION_KEY]);
  const classification = extra?.classification;
  const era: Era =
    classification?.era ??
    (claimed === undefined ? (connection.settings.era ?? "legacy") : "modern");
  const request = extra?.request;
  const id = "id" in message ? message.id : undefined;
  if (message.method === "initialize") {
    connection.client = implementation(params?.clientInfo);
  }

  const attributes: Attributes = {
    "mcp.method.name": message.method,
    "crucible.mcp.capture_source": "sdk",
    "crucible.mcp.era": era,
    "crucible.mcp.transport":
      options.transport ?? (request === undefined ? connection.label : "streamable_http"),
    ...targetAttributes(message.method, params),
  };
  if (id === undefined) {
    attributes["crucible.mcp.is_notification"] = true;
  } else {
    attributes["jsonrpc.request.id"] = String(id);
    if (message.method === "subscriptions/listen") {
      attributes["crucible.mcp.subscription.id"] = String(id);
    }
  }
  put(
    attributes,
    "mcp.protocol.version",
    claimed ??
      classification?.revision ??
      request?.headers.get("mcp-protocol-version") ??
      connection.protocolVersion,
  );
  putImplementation(
    attributes,
    "crucible.mcp.client",
    implementation(meta?.[CLIENT_INFO_KEY]) ?? connection.client,
  );
  if (request !== undefined) {
    put(attributes, "user_agent.original", request.headers.get("user-agent") ?? undefined);
    const length = Number(request.headers.get("content-length") ?? Number.NaN);
    if (Number.isSafeInteger(length) && length >= 0) {
      attributes["crucible.mcp.request.bytes"] = length;
    }
  }
  const state = text(params?.requestState);
  const key = secretKey();
  if (state !== undefined && key !== undefined) {
    attributes["crucible.mcp.mrtr.state_in"] = stateHash(key, state);
  }

  const tool = message.method === "tools/call" ? text(params?.name) : undefined;
  const capture = tool === undefined ? undefined : decide(options.capture, tool);
  let cut = false;
  if (capture?.args === true) {
    const args = captured(params?.arguments ?? {}, capture.maxBytes);
    if (args !== undefined) {
      attributes["gen_ai.tool.call.arguments"] = args.text;
      cut = args.cut;
    }
  }
  const handleParam = tool === undefined ? undefined : options.handleParams?.[tool];
  const handleValue =
    handleParam === undefined ? undefined : text(fields(params?.arguments)?.[handleParam]);

  const annotations: Annotations = {};
  const hooked = hookIdentity(options, extra);
  if (hooked !== undefined) {
    annotations.identity = hooked;
  }
  const parent = propagator.extract(ROOT_CONTEXT, meta ?? {}, metaGetter);
  const span = tracer().startSpan(
    spanName(message.method, params),
    { kind: SpanKind.SERVER, attributes },
    parent,
  );
  return {
    span,
    method: message.method,
    id,
    era,
    http: request !== undefined,
    authInfo: extra?.authInfo,
    annotations,
    handle: handleId(handleValue),
    capture,
    cut,
    progress: 0,
  };
}

function outbound(
  message: JSONRPCMessage,
  relatedRequestId: RequestId | undefined,
  connection: Connection,
): void {
  if ("method" in message) {
    if (message.method === "notifications/progress" && relatedRequestId !== undefined) {
      const related = connection.pending.get(relatedRequestId);
      if (related !== undefined) {
        related.progress += 1;
      }
    }
    return;
  }
  const id = message.id;
  const operation = id === undefined || id === null ? undefined : connection.pending.get(id);
  if (operation === undefined || id === undefined || id === null) {
    return;
  }
  connection.pending.delete(id);
  const result = applyResponse(operation.span, message, operation.annotations);
  if (result !== undefined) {
    observeResult(operation, result, connection);
  }
  finish(operation, connection);
}

function observeResult(operation: Operation, result: Fields, connection: Connection): void {
  if (operation.method === "initialize") {
    connection.protocolVersion = text(result.protocolVersion);
    connection.settings.shared.server =
      implementation(result.serverInfo) ?? connection.settings.shared.server;
    if (connection.protocolVersion !== undefined) {
      operation.span.setAttribute("mcp.protocol.version", connection.protocolVersion);
    }
  }
  const announced = implementation(fields(result._meta)?.[SERVER_INFO_KEY]);
  if (announced !== undefined) {
    connection.settings.shared.server = announced;
  }
  const key = secretKey();
  const state = text(result.requestState);
  if (result.resultType === "input_required" && state !== undefined && key !== undefined) {
    operation.span.setAttribute("crucible.mcp.mrtr.state_out", stateHash(key, state));
  }
  if (operation.method !== "tools/call") {
    return;
  }
  if (Array.isArray(result.content)) {
    const blocks = new Map<string, number>();
    for (const block of result.content) {
      const type = text(fields(block)?.type);
      if (type !== undefined) {
        blocks.set(type, (blocks.get(type) ?? 0) + 1);
      }
    }
    for (const [type, count] of blocks) {
      operation.span.setAttribute(`crucible.mcp.content.blocks.${type}`, count);
    }
  }
  if (operation.capture?.result === true) {
    // `requestState` is never emitted (contract 3.5), even inside captured content.
    const { requestState, ...rest } = result;
    void requestState;
    const content = captured(rest, operation.capture.maxBytes);
    if (content !== undefined) {
      operation.span.setAttribute("gen_ai.tool.call.result", content.text);
      operation.cut ||= content.cut;
    }
  }
}

/** The client cancelled a request. The SDK sends no response for it, so it is finished here. */
function cancel(requestId: unknown, connection: Connection): void {
  if (typeof requestId !== "string" && typeof requestId !== "number") {
    return;
  }
  const operation = connection.pending.get(requestId);
  if (operation === undefined) {
    return;
  }
  connection.pending.delete(requestId);
  operation.span.setAttribute("crucible.mcp.cancel.source", "notification");
  finish(operation, connection);
}

/** The transport closed with requests still unanswered. */
function abandon(connection: Connection): void {
  for (const operation of connection.pending.values()) {
    applyClosed(operation.span, operation.era === "modern" && operation.http);
    finish(operation, connection);
  }
  connection.pending.clear();
}

function finish(operation: Operation, connection: Connection): void {
  const attributes: Attributes = {};
  if (operation.id !== undefined) {
    attributes["crucible.mcp.progress.count"] = operation.progress;
  }
  const sessionId = connection.transport.sessionId;
  if (operation.era === "legacy" && sessionId !== undefined) {
    attributes["mcp.session.id"] = sessionKey(sessionId);
  }
  putImplementation(attributes, "crucible.mcp.server", connection.settings.shared.server);
  Object.assign(
    attributes,
    identityAttributes(
      connection.settings.options,
      operation.annotations.identity,
      operation.authInfo,
      operation.handle,
    ),
  );
  if (operation.capture !== undefined) {
    attributes["crucible.mcp.content.captured"] = true;
    attributes["crucible.mcp.content.sample_rate"] = operation.capture.sampleRate;
    attributes["crucible.mcp.content.truncation"] = operation.cut ? "cap_exceeded" : "none";
  }
  operation.span.setAttributes(attributes);
  operation.span.end();
}
