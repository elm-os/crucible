// Exact expectations for the operations of the standard workload (`exercise`), per scenario.

import { createHash } from "node:crypto";
import { expect } from "vitest";
import type { DecodedSpan, Value } from "./otlp.ts";
import { SERVER_NAME, SERVER_VERSION } from "./reference-server.ts";
import { CLIENT_NAME, CLIENT_VERSION, type Seen } from "./scenarios.ts";

export const OK = { code: 0, message: "" };
export const ERROR = { code: 2, message: "" };
export const SERVER = 2;
export const CLIENT = 3;

export interface Op {
  name: string;
  kind: number;
  status: { code: number; message: string };
  attributes: Record<string, Value>;
}

/** The operations of `spans`, comparable with `toEqual`, in a stable order. */
export function ops(spans: DecodedSpan[]): Op[] {
  return sortOps(
    spans
      .filter((span) => "mcp.method.name" in span.attributes)
      .map((span) => ({
        name: span.name,
        kind: span.kind,
        status: span.status,
        attributes: span.attributes,
      })),
  );
}

export function sortOps(list: Op[]): Op[] {
  const key = (op: Op): string =>
    `${String(op.attributes["jsonrpc.request.id"] ?? "~")}|${op.name}|${String(op.attributes["user.id"] ?? "")}`;
  return list.toSorted((a, b) => key(a).localeCompare(key(b)));
}

export function sessionKey(raw: string): string {
  return createHash("sha256").update(raw).digest("hex").slice(0, 32);
}

export interface Shape {
  era: "legacy" | "modern";
  transport: "stdio" | "streamable_http" | "unknown";
  protocolVersion: string;
  /** JSON-RPC ids in order: initialize (legacy only), list, echo, fail, throw, slow alpha, slow beta, missing. */
  ids: string[];
  /** Which operations carry client info. */
  client: "all" | "initialize";
  /** Which operations carry server info. */
  server: "all" | "initialize" | "results";
  /** Raw session id, legacy session HTTP only. */
  sessionId?: string;
  /** Request sizes and user agents, HTTP only. */
  seen?: Seen;
  /** A leading `server/discover` probe (modern HTTP), with its id. */
  discover?: string;
}

/** The exact operations the standard workload produces for `shape`. */
export function workload(shape: Shape): Op[] {
  const legacy = shape.era === "legacy";
  const ids = [...shape.ids];
  const take = (): string => {
    const id = ids.shift();
    if (id === undefined) {
      throw new Error("not enough ids");
    }
    return id;
  };
  const list: Op[] = [];
  const op = (
    name: string,
    method: string,
    id: string | undefined,
    extra: Record<string, Value>,
    status: Op["status"] = OK,
    isResult = true,
  ): void => {
    const initialize = method === "initialize";
    const attributes: Record<string, Value> = {
      "mcp.method.name": method,
      "crucible.mcp.capture_source": "sdk",
      "crucible.mcp.era": shape.era,
      "crucible.mcp.transport": shape.transport,
      "mcp.protocol.version": shape.protocolVersion,
    };
    if (id === undefined) {
      attributes["crucible.mcp.is_notification"] = true;
    } else {
      attributes["jsonrpc.request.id"] = id;
      attributes["crucible.mcp.progress.count"] = 0n;
    }
    if (shape.client === "all" || initialize) {
      attributes["crucible.mcp.client.name"] = CLIENT_NAME;
      attributes["crucible.mcp.client.version"] = CLIENT_VERSION;
    }
    if (shape.server === "all" || initialize || (shape.server === "results" && isResult)) {
      attributes["crucible.mcp.server.name"] = SERVER_NAME;
      attributes["crucible.mcp.server.version"] = SERVER_VERSION;
    }
    if (shape.sessionId !== undefined) {
      attributes["mcp.session.id"] = sessionKey(shape.sessionId);
    }
    const seen = shape.seen?.get(id ?? method);
    if (seen !== undefined) {
      attributes["crucible.mcp.request.bytes"] = BigInt(seen.bytes);
      if (seen.userAgent !== null) {
        attributes["user_agent.original"] = seen.userAgent;
      }
    }
    list.push({ name, kind: SERVER, status, attributes: { ...attributes, ...extra } });
  };
  const complete = { "crucible.mcp.result_type": "complete" };
  const tool = (name: string, extra: Record<string, Value> = {}): Record<string, Value> => ({
    "gen_ai.tool.name": name,
    "crucible.mcp.content.blocks.text": 1n,
    ...complete,
    ...extra,
  });
  if (shape.discover !== undefined) {
    op("server/discover", "server/discover", shape.discover, complete);
  }
  if (legacy) {
    op("initialize", "initialize", take(), complete);
    op("notifications/initialized", "notifications/initialized", undefined, {}, OK, false);
  }
  op("tools/list", "tools/list", take(), complete);
  op("tools/call echo", "tools/call", take(), tool("echo"));
  op(
    "tools/call fail",
    "tools/call",
    take(),
    tool("fail", { "error.type": "tool_error", "crucible.mcp.error.subclass": "business" }),
    ERROR,
  );
  // A thrown handler error reaches the wire as a tool error too (a known capability gap).
  op(
    "tools/call throw",
    "tools/call",
    take(),
    tool("throw", { "error.type": "tool_error" }),
    ERROR,
  );
  const slow = { "crucible.mcp.identity.source": "identify_hook" };
  op("tools/call slow", "tools/call", take(), tool("slow", { ...slow, "user.id": "alpha" }));
  op("tools/call slow", "tools/call", take(), tool("slow", { ...slow, "user.id": "beta" }));
  op(
    "tools/call missing",
    "tools/call",
    take(),
    {
      "gen_ai.tool.name": "missing",
      "rpc.response.status_code": "-32602",
      "error.type": "-32602",
    },
    { code: 2, message: "Tool missing not found" },
    false,
  );
  return sortOps(list);
}

function ms(nanos: bigint): number {
  return Number(nanos / 1_000_000n);
}

/** The two slow calls overlapped, and each span covers its handler's wait. */
export function expectSlowOverlap(spans: DecodedSpan[]): void {
  const slow = (tag: string): DecodedSpan => {
    const found = spans.find(
      (span) =>
        span.attributes["gen_ai.tool.name"] === "slow" && span.attributes["user.id"] === tag,
    );
    if (found === undefined) {
      throw new Error(`no slow call tagged ${tag}`);
    }
    return found;
  };
  const alpha = slow("alpha");
  const beta = slow("beta");
  expect(beta.startTimeUnixNano < alpha.endTimeUnixNano).toBe(true);
  expect(alpha.startTimeUnixNano < beta.endTimeUnixNano).toBe(true);
  expect(ms(alpha.endTimeUnixNano - alpha.startTimeUnixNano)).toBeGreaterThanOrEqual(140);
  const betaMs = ms(beta.endTimeUnixNano - beta.startTimeUnixNano);
  expect(betaMs).toBeGreaterThanOrEqual(50);
  expect(betaMs).toBeLessThan(140);
}
