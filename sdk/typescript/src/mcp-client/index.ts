// `@elm-os/crucible/mcp-client`: wraps an MCP client so that each request it sends becomes a CLIENT span and
// carries that span's W3C trace context in `params._meta` (`traceparent`, `tracestate`; contract 9.1). A server
// instrumented with `@elm-os/crucible/mcp` then records its span as a child of the client's.

import type { JSONRPCMessage, Transport, TransportSendOptions } from "@modelcontextprotocol/client";
import {
  type Attributes,
  defaultTextMapSetter,
  ROOT_CONTEXT,
  type Span,
  SpanKind,
  trace,
} from "@opentelemetry/api";
import {
  applyClosed,
  applyResponse,
  CLIENT_INFO_KEY,
  fields,
  type Implementation,
  implementation,
  interpose,
  PROTOCOL_VERSION_KEY,
  propagator,
  put,
  putImplementation,
  SERVER_INFO_KEY,
  safely,
  spanName,
  type TransportLabel,
  tappable,
  targetAttributes,
  text,
} from "../mcp/wire.ts";
import { contextAttributes, parentContext } from "../scope.ts";
import { tracer } from "../setup.ts";

export type { TransportLabel } from "../mcp/wire.ts";

export interface InstrumentClientOptions {
  /** Transport label. By default a per-request HTTP transport is `streamable_http`, anything else `unknown`. */
  transport?: TransportLabel;
}

/** The part of an MCP client this wrapper uses. */
export interface ConnectableClient {
  connect(transport: Transport, options?: unknown): Promise<void>;
}

type RequestId = string | number;

interface Connection {
  readonly label: TransportLabel;
  readonly pending: Map<RequestId, Span>;
  client: Implementation | undefined;
  server: Implementation | undefined;
  protocolVersion: string | undefined;
}

const instrumented = new WeakSet<object>();
const tapped = new WeakSet<Transport>();

/** Instruments an MCP client and returns it. A transport that cannot be tapped is left uninstrumented. */
export function instrumentClient<C extends ConnectableClient>(
  client: C,
  options: InstrumentClientOptions = {},
): C {
  if (instrumented.has(client)) {
    return client;
  }
  try {
    const target: ConnectableClient = client;
    const connect = target.connect.bind(target);
    target.connect = (transport, connectOptions) => {
      safely(() => tap(transport, options));
      return connect(transport, connectOptions);
    };
    instrumented.add(client);
  } catch {
    // A frozen client stays uninstrumented.
  }
  return client;
}

function tap(transport: Transport, options: InstrumentClientOptions): void {
  if (tapped.has(transport) || !tappable(transport, ["onmessage", "onclose"], "send")) {
    return;
  }
  tapped.add(transport);
  const connection: Connection = {
    label:
      options.transport ?? (transport.hasPerRequestStream === true ? "streamable_http" : "unknown"),
    pending: new Map(),
    client: undefined,
    server: undefined,
    protocolVersion: undefined,
  };
  const send = transport.send.bind(transport);
  transport.send = (message, sendOptions) => {
    let outgoing = message;
    safely(() => {
      outgoing = outbound(message, sendOptions, connection);
    });
    return send(outgoing, sendOptions);
  };
  interpose(transport, "onmessage", (next) => (message, extra) => {
    safely(() => inbound(message, connection));
    next(message, extra);
  });
  interpose(transport, "onclose", (next) => () => {
    safely(() => {
      for (const span of connection.pending.values()) {
        applyClosed(span, false);
        finish(span, connection);
      }
      connection.pending.clear();
    });
    next();
  });
}

/** Starts the span of an outgoing request and returns the request with the span's trace context injected. */
function outbound(
  message: JSONRPCMessage,
  sendOptions: TransportSendOptions | undefined,
  connection: Connection,
): JSONRPCMessage {
  if (!("method" in message)) {
    return message;
  }
  if (!("id" in message)) {
    const requestId = fields(message.params)?.requestId;
    if (
      message.method === "notifications/cancelled" &&
      (typeof requestId === "string" || typeof requestId === "number")
    ) {
      const span = connection.pending.get(requestId);
      if (span !== undefined) {
        connection.pending.delete(requestId);
        span.setAttribute("crucible.mcp.cancel.source", "notification");
        finish(span, connection);
      }
    }
    return message;
  }
  const params = fields(message.params) ?? {};
  const meta = fields(params._meta) ?? {};
  const claimed = text(meta[PROTOCOL_VERSION_KEY]);
  if (message.method === "initialize") {
    connection.client = implementation(params.clientInfo);
  }
  const attributes: Attributes = {
    ...contextAttributes(),
    "mcp.method.name": message.method,
    "crucible.mcp.capture_source": "sdk",
    "crucible.mcp.era": claimed === undefined ? "legacy" : "modern",
    "crucible.mcp.transport": connection.label,
    "jsonrpc.request.id": String(message.id),
    ...targetAttributes(message.method, params),
  };
  put(attributes, "mcp.protocol.version", claimed ?? connection.protocolVersion);
  putImplementation(
    attributes,
    "crucible.mcp.client",
    implementation(meta[CLIENT_INFO_KEY]) ?? connection.client,
  );
  const span = tracer().startSpan(
    spanName(message.method, params),
    { kind: SpanKind.CLIENT, attributes },
    parentContext(),
  );
  const id = message.id;
  connection.pending.set(id, span);
  // On a per-request HTTP stream (2026-07-28) the client cancels a request by aborting its stream.
  sendOptions?.requestSignal?.addEventListener(
    "abort",
    () =>
      safely(() => {
        if (connection.pending.get(id) === span) {
          connection.pending.delete(id);
          applyClosed(span, true);
          finish(span, connection);
        }
      }),
    { once: true },
  );
  const carrier: Record<string, string> = {};
  propagator.inject(trace.setSpan(ROOT_CONTEXT, span), carrier, defaultTextMapSetter);
  if (Object.keys(carrier).length === 0) {
    return message;
  }
  return { ...message, params: { ...params, _meta: { ...meta, ...carrier } } };
}

function inbound(message: JSONRPCMessage, connection: Connection): void {
  if ("method" in message) {
    return;
  }
  const id = message.id;
  const span = id === undefined || id === null ? undefined : connection.pending.get(id);
  if (span === undefined || id === undefined || id === null) {
    return;
  }
  connection.pending.delete(id);
  const result = applyResponse(span, message, undefined);
  if (result !== undefined) {
    const protocolVersion = text(result.protocolVersion);
    if (protocolVersion !== undefined && result.serverInfo !== undefined) {
      connection.protocolVersion = protocolVersion;
      span.setAttribute("mcp.protocol.version", protocolVersion);
    }
    connection.server =
      implementation(fields(result._meta)?.[SERVER_INFO_KEY]) ??
      implementation(result.serverInfo) ??
      connection.server;
  }
  finish(span, connection);
}

function finish(span: Span, connection: Connection): void {
  const attributes: Attributes = {};
  putImplementation(attributes, "crucible.mcp.server", connection.server);
  span.setAttributes(attributes);
  span.end();
}
