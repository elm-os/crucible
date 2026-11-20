// An in-process fake OTLP/HTTP collector. It accepts only `POST /v1/traces` with a protobuf body and the expected
// Bearer key, decodes the spans, and can be switched to stall (never answer) or fail (503). It also answers
// `POST /api/v1/projects/{id}/feedback`, from `feedbackAnswers` while that list holds any, else with 201.

import { createServer, type IncomingHttpHeaders } from "node:http";
import type { AddressInfo, Socket } from "node:net";
import { setTimeout as sleep } from "node:timers/promises";
import { type DecodedSpan, decodeTraces } from "./otlp.ts";

export interface CollectedRequest {
  method: string;
  path: string;
  headers: IncomingHttpHeaders;
  body: Buffer;
}

export type CollectorMode = "accept" | "stall" | "fail";

/** One answer to a feedback post. Status 0 closes the connection without answering. */
export interface FeedbackAnswer {
  status: number;
  headers?: Record<string, string>;
  body?: string;
}

export interface Collector {
  /** Base URL, without a path. */
  readonly url: string;
  readonly key: string;
  /** Every request, accepted or not. */
  readonly requests: CollectedRequest[];
  /** Spans of accepted requests, in arrival order. */
  readonly spans: DecodedSpan[];
  mode: CollectorMode;
  /** Answers to the next feedback posts, taken in order. */
  readonly feedbackAnswers: FeedbackAnswer[];
  /** Resolves with the spans once `predicate` holds; rejects after `timeoutMs`. */
  waitFor(predicate: (spans: DecodedSpan[]) => boolean, timeoutMs?: number): Promise<DecodedSpan[]>;
  close(): Promise<void>;
}

export const TEST_KEY = "csk_0TestKeyForTheFakeCollector00000000000000";

export async function startCollector(key: string = TEST_KEY): Promise<Collector> {
  const requests: CollectedRequest[] = [];
  const spans: DecodedSpan[] = [];
  const sockets = new Set<Socket>();
  const collector: Collector = {
    url: "",
    key,
    requests,
    spans,
    mode: "accept",
    feedbackAnswers: [],
    async waitFor(predicate, timeoutMs = 10_000) {
      const deadline = Date.now() + timeoutMs;
      while (!predicate(spans)) {
        if (Date.now() > deadline) {
          throw new Error(
            `collector: condition not met after ${timeoutMs} ms; spans: ${spans.map((s) => s.name).join(", ")}`,
          );
        }
        await sleep(10);
      }
      return spans;
    },
    close: () =>
      new Promise<void>((resolve) => {
        for (const socket of sockets) {
          socket.destroy();
        }
        server.close(() => resolve());
      }),
  };
  const server = createServer(async (req, res) => {
    const chunks: Buffer[] = [];
    for await (const chunk of req) {
      chunks.push(chunk as Buffer);
    }
    const body = Buffer.concat(chunks);
    requests.push({ method: req.method ?? "", path: req.url ?? "", headers: req.headers, body });
    if (collector.mode === "stall") {
      return;
    }
    if (collector.mode === "fail") {
      res.writeHead(503).end();
      return;
    }
    if (req.method === "POST" && /^\/api\/v1\/projects\/[^/]+\/feedback$/.test(req.url ?? "")) {
      const answer = collector.feedbackAnswers.shift() ?? {
        status: 201,
        headers: { "content-type": "application/json" },
        body: '{"fact_id":"01a1f65b-b4a4-70eb-83eb-6dc856127f2d","key":"k","version":1}',
      };
      if (answer.status === 0) {
        req.socket.destroy();
        return;
      }
      res.writeHead(answer.status, answer.headers).end(answer.body);
      return;
    }
    if (req.method !== "POST" || req.url !== "/v1/traces") {
      res.writeHead(404).end();
      return;
    }
    if (req.headers["content-type"] !== "application/x-protobuf") {
      res.writeHead(415).end();
      return;
    }
    if (req.headers.authorization !== `Bearer ${key}`) {
      res.writeHead(401).end();
      return;
    }
    spans.push(...decodeTraces(body));
    res.writeHead(200, { "content-type": "application/x-protobuf" }).end();
  });
  server.on("connection", (socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  Object.assign(collector, { url: `http://127.0.0.1:${port}` });
  return collector;
}

/** The spans that are MCP operations. */
export function operations(spans: DecodedSpan[]): DecodedSpan[] {
  return spans.filter((span) => "mcp.method.name" in span.attributes);
}
