// Spike: attachment seam for MCP servers built on the official TypeScript MCP SDK (2.x).
//
// Candidate A (`instrument`, `instrumentFactory`): replace the instance's public `connect` so that
// every transport handed to it gets a tap on the public members of the `Transport` interface
// (`onmessage`, `send`, `onclose`). The transport object itself is kept, because the SDK checks the
// class of the transport it is connected to and keeps its own reference to it.
//
// Candidate B (`instrumentHandler`): wrap the fetch face of the HTTP handler. It records only the
// exchanges that never reach `connect` (rejections at the HTTP edge and subscription streams).

import { AsyncLocalStorage } from "node:async_hooks";
import { performance } from "node:perf_hooks";
import type {
  JSONRPCMessage,
  JSONRPCNotification,
  JSONRPCRequest,
  McpHandlerRequestOptions,
  McpRequestContext,
  McpServer,
  MessageExtraInfo,
  Server,
  Transport,
} from "@modelcontextprotocol/server";
import { CLIENT_INFO_META_KEY, PROTOCOL_VERSION_META_KEY } from "@modelcontextprotocol/server";

export type Era = "legacy" | "modern";
export type Outcome = "ok" | "tool_error" | "protocol_error";
export type ErrorSubclass = "auth" | "rate_limit" | "upstream" | "business";

export interface Identity {
  userId?: string;
  issuer?: string;
  subject?: string;
  tenant?: string;
  handleId?: string;
}

export interface Operation {
  /** `connect`: seen by the transport tap. `edge`: seen only by the fetch wrap. */
  source: "connect" | "edge";
  kind: "request" | "notification";
  method: string;
  target?: string;
  jsonrpcId?: string | number;
  era: Era;
  protocolVersion?: string;
  transport: string;
  sessionId?: string;
  clientName?: string;
  clientVersion?: string;
  startMs: number;
  durationMs: number;
  outcome: Outcome;
  errorCode?: number;
  errorType?: string;
  errorSubclass?: ErrorSubclass;
  identity?: Identity;
  progressCount: number;
  httpStatus?: number;
}

export type Sink = (operation: Operation) => void;

export interface InstrumentOptions {
  sink: Sink;
  /** Era hint from the factory context. The per-message classification wins when present. */
  era?: Era;
  /** Transport label. Defaults to the class name of the connected transport. */
  transport?: string;
  /** Called once per inbound message, before dispatch. */
  identify?: (extra: MessageExtraInfo | undefined) => Identity | undefined;
}

type Product = McpServer | Server;
type Dispatch = NonNullable<Transport["onmessage"]>;
type Fields = Record<string, unknown>;

interface Connection {
  pending: Map<string | number, Operation>;
  clientName: string | undefined;
  clientVersion: string | undefined;
  protocolVersion: string | undefined;
}

interface EdgeScope {
  seen: number;
}

const activeOperation = new AsyncLocalStorage<Operation>();
const edgeScope = new AsyncLocalStorage<EdgeScope>();
const instrumented = new WeakSet<Product>();
const tapped = new WeakSet<Transport>();

/** Records an error subclass on the operation that is being handled. No-op outside a handler. */
export function annotateError(subclass: ErrorSubclass): void {
  const operation = activeOperation.getStore();
  if (operation !== undefined) {
    operation.errorSubclass = subclass;
  }
}

/** Merges identity fields into the operation that is being handled. No-op outside a handler. */
export function identify(identity: Identity): void {
  const operation = activeOperation.getStore();
  if (operation !== undefined) {
    operation.identity = { ...operation.identity, ...identity };
  }
}

/** Candidate A: decorate the public `connect` of one server instance. */
export function instrument<S extends Product>(server: S, options: InstrumentOptions): S {
  if (instrumented.has(server)) {
    return server;
  }
  instrumented.add(server);
  const connection: Connection = {
    pending: new Map(),
    clientName: undefined,
    clientVersion: undefined,
    protocolVersion: undefined,
  };
  const connect = server.connect.bind(server);
  server.connect = (transport: Transport): Promise<void> => {
    tap(transport, connection, options);
    return connect(transport);
  };
  return server;
}

/** Candidate A for the factory entries (`createMcpHandler`, `serveStdio`). */
export function instrumentFactory(
  factory: (ctx: McpRequestContext) => Product | Promise<Product>,
  options: InstrumentOptions,
): (ctx: McpRequestContext) => Promise<Product> {
  return async (ctx) => instrument(await factory(ctx), { ...options, era: ctx.era });
}

function tap(transport: Transport, connection: Connection, options: InstrumentOptions): void {
  if (tapped.has(transport)) {
    return;
  }
  tapped.add(transport);
  const label = options.transport ?? transport.constructor.name;

  // The SDK assigns `onmessage` and `onclose` during connect. Accessors installed before that
  // assignment mean no inbound message can be dispatched around the tap.
  interpose(transport, "onmessage", (next) =>
    wrapDispatch(next, transport, connection, options, label),
  );
  interpose(transport, "onclose", (next) => () => {
    abandon(connection, transport, options.sink);
    next();
  });

  const send = transport.send.bind(transport);
  transport.send = (message, sendOptions) => {
    observeOutbound(message, sendOptions?.relatedRequestId, transport, connection, options.sink);
    return send(message, sendOptions);
  };
}

/**
 * Routes assignments of one callback member through `wrap`. Some transports keep the callback in
 * a plain field; others forward it to an inner transport through an accessor on their class. An
 * existing accessor is kept in the path, so the forwarding still happens.
 */
function interpose<K extends "onmessage" | "onclose">(
  transport: Transport,
  key: K,
  wrap: (next: NonNullable<Transport[K]>) => NonNullable<Transport[K]>,
): void {
  const inherited = findAccessor(transport, key);
  let current: Transport[K] = inherited === undefined ? transport[key] : undefined;
  Object.defineProperty(transport, key, {
    configurable: true,
    enumerable: true,
    get: (): Transport[K] =>
      inherited?.get === undefined ? current : inherited.get.call(transport),
    set: (next: Transport[K]) => {
      const wrapped = next === undefined || next === null ? undefined : wrap(next);
      if (inherited?.set === undefined) {
        current = wrapped;
      } else {
        inherited.set.call(transport, wrapped);
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

function wrapDispatch(
  next: Dispatch,
  transport: Transport,
  connection: Connection,
  options: InstrumentOptions,
  label: string,
): Dispatch {
  return (message, extra) => {
    if (!("method" in message)) {
      // A client response to a server-initiated request: not an operation of this server.
      next(message, extra);
      return;
    }
    if (message.method === "notifications/cancelled") {
      cancel(fields(message.params)?.requestId, transport, connection, options.sink);
    }
    const operation = begin(message, extra, connection, options, label);
    const scope = edgeScope.getStore();
    if (scope !== undefined) {
      scope.seen += 1;
    }
    if (operation.kind === "request" && operation.jsonrpcId !== undefined) {
      connection.pending.set(operation.jsonrpcId, operation);
      activeOperation.run(operation, next, message, extra);
      return;
    }
    try {
      activeOperation.run(operation, next, message, extra);
    } finally {
      finish(operation, transport, options.sink);
    }
  };
}

function begin(
  message: JSONRPCRequest | JSONRPCNotification,
  extra: MessageExtraInfo | undefined,
  connection: Connection,
  options: InstrumentOptions,
  label: string,
): Operation {
  const params = fields(message.params);
  const meta = fields(params?._meta);
  const claimed = text(meta?.[PROTOCOL_VERSION_META_KEY]);
  const classification = extra?.classification;
  const operation: Operation = {
    source: "connect",
    kind: "id" in message ? "request" : "notification",
    method: message.method,
    era: classification?.era ?? (claimed === undefined ? (options.era ?? "legacy") : "modern"),
    transport: label,
    startMs: performance.now(),
    durationMs: 0,
    outcome: "ok",
    progressCount: 0,
  };
  if ("id" in message && message.id !== undefined) {
    operation.jsonrpcId = message.id;
  }
  const target = targetOf(message.method, params);
  if (target !== undefined) {
    operation.target = target;
  }

  if (message.method === "initialize") {
    const info = fields(params?.clientInfo);
    connection.clientName = text(info?.name);
    connection.clientVersion = text(info?.version);
  }
  const envelopeInfo = fields(meta?.[CLIENT_INFO_META_KEY]);
  const clientName = text(envelopeInfo?.name) ?? connection.clientName;
  const clientVersion = text(envelopeInfo?.version) ?? connection.clientVersion;
  if (clientName !== undefined) {
    operation.clientName = clientName;
  }
  if (clientVersion !== undefined) {
    operation.clientVersion = clientVersion;
  }

  const protocolVersion =
    claimed ??
    classification?.revision ??
    extra?.request?.headers.get("mcp-protocol-version") ??
    connection.protocolVersion;
  if (protocolVersion !== undefined) {
    operation.protocolVersion = protocolVersion;
  }

  const identity = options.identify?.(extra);
  if (identity !== undefined) {
    operation.identity = identity;
  }
  return operation;
}

function observeOutbound(
  message: JSONRPCMessage,
  relatedRequestId: string | number | undefined,
  transport: Transport,
  connection: Connection,
  sink: Sink,
): void {
  if ("method" in message) {
    if (message.method === "notifications/progress" && relatedRequestId !== undefined) {
      const related = connection.pending.get(relatedRequestId);
      if (related !== undefined) {
        related.progressCount += 1;
      }
    }
    return;
  }
  if (message.id === undefined || message.id === null) {
    return;
  }
  const operation = connection.pending.get(message.id);
  if (operation === undefined) {
    return;
  }
  connection.pending.delete(message.id);
  if ("error" in message) {
    operation.outcome = "protocol_error";
    operation.errorCode = message.error.code;
  } else {
    const result = fields(message.result);
    if (result?.isError === true) {
      operation.outcome = "tool_error";
    }
    if (operation.method === "initialize") {
      connection.protocolVersion = text(result?.protocolVersion);
      if (connection.protocolVersion !== undefined) {
        operation.protocolVersion = connection.protocolVersion;
      }
    }
  }
  finish(operation, transport, sink);
}

/** The client cancelled a request. The SDK sends no response for it, so it is finished here. */
function cancel(
  requestId: unknown,
  transport: Transport,
  connection: Connection,
  sink: Sink,
): void {
  if (typeof requestId !== "string" && typeof requestId !== "number") {
    return;
  }
  const operation = connection.pending.get(requestId);
  if (operation === undefined) {
    return;
  }
  connection.pending.delete(requestId);
  operation.outcome = "protocol_error";
  operation.errorType = "cancelled";
  finish(operation, transport, sink);
}

/** The transport closed with requests still unanswered. */
function abandon(connection: Connection, transport: Transport, sink: Sink): void {
  for (const operation of connection.pending.values()) {
    operation.outcome = "protocol_error";
    operation.errorType = "connection_closed";
    finish(operation, transport, sink);
  }
  connection.pending.clear();
}

function finish(operation: Operation, transport: Transport, sink: Sink): void {
  operation.durationMs = performance.now() - operation.startMs;
  if (transport.sessionId !== undefined) {
    operation.sessionId = transport.sessionId;
  }
  try {
    sink(operation);
  } catch {
    // Telemetry never affects the wire.
  }
}

interface FetchFace {
  fetch: (request: Request, options?: McpHandlerRequestOptions) => Promise<Response>;
}

/**
 * Candidate B: wrap the fetch face. When the transport tap saw the exchange, this adds nothing.
 * Otherwise it records one `edge` operation from the request body and the response.
 */
export function instrumentHandler<H extends FetchFace>(
  handler: H,
  options: Pick<InstrumentOptions, "sink">,
): H {
  const inner = handler.fetch;
  const fetch: FetchFace["fetch"] = async (request, requestOptions) => {
    const scope: EdgeScope = { seen: 0 };
    const startMs = performance.now();
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
    const responseProbe = isJson ? response.clone() : undefined;
    void recordEdge(
      request,
      requestOptions?.parsedBody,
      probe,
      response.status,
      responseProbe,
      startMs,
      options.sink,
    );
    return response;
  };
  return { ...handler, fetch };
}

async function recordEdge(
  request: Request,
  parsedBody: unknown,
  probe: Request | undefined,
  httpStatus: number,
  responseProbe: Response | undefined,
  startMs: number,
  sink: Sink,
): Promise<void> {
  const body = fields(parsedBody ?? (await readJson(probe)));
  const params = fields(body?.params);
  const meta = fields(params?._meta);
  const claimed = text(meta?.[PROTOCOL_VERSION_META_KEY]);
  const method = text(body?.method) ?? `http.${request.method.toLowerCase()}`;
  const operation: Operation = {
    source: "edge",
    kind: body !== undefined && !("id" in body) ? "notification" : "request",
    method,
    era: claimed === undefined ? "legacy" : "modern",
    transport: "http",
    startMs,
    durationMs: performance.now() - startMs,
    outcome: httpStatus >= 400 ? "protocol_error" : "ok",
    progressCount: 0,
    httpStatus,
  };
  const id = body?.id;
  if (typeof id === "string" || typeof id === "number") {
    operation.jsonrpcId = id;
  }
  const target = targetOf(method, params);
  if (target !== undefined) {
    operation.target = target;
  }
  const protocolVersion = claimed ?? request.headers.get("mcp-protocol-version") ?? undefined;
  if (protocolVersion !== undefined) {
    operation.protocolVersion = protocolVersion;
  }
  const info = fields(meta?.[CLIENT_INFO_META_KEY]);
  const clientName = text(info?.name);
  if (clientName !== undefined) {
    operation.clientName = clientName;
  }
  const error = fields(fields(await readJson(responseProbe))?.error);
  if (error !== undefined) {
    operation.outcome = "protocol_error";
    if (typeof error.code === "number") {
      operation.errorCode = error.code;
    }
  }
  try {
    sink(operation);
  } catch {
    // Telemetry never affects the wire.
  }
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

function targetOf(method: string, params: Fields | undefined): string | undefined {
  if (method === "tools/call" || method === "prompts/get") {
    return text(params?.name);
  }
  if (method.startsWith("resources/")) {
    return text(params?.uri);
  }
  return undefined;
}

function fields(value: unknown): Fields | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Fields)
    : undefined;
}

function text(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}
