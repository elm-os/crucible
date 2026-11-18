// Latency gate for the seam spike: added latency per tools/call, measured as paired runs with and
// without instrumentation. Rounds alternate between the variants so drift hits both equally.
// Usage: node src/bench.ts [--calls=100000] [--rounds=40] [--loopback-calls=2000]

import { performance } from "node:perf_hooks";
import { Client, StreamableHTTPClientTransport } from "@modelcontextprotocol/client";
import {
  createMcpHandler,
  InMemoryTransport,
  PROTOCOL_VERSION_META_KEY,
} from "@modelcontextprotocol/server";
import { host } from "./http-host.ts";
import { instrument, instrumentFactory, instrumentHandler, type Operation } from "./instrument.ts";
import { buildServer } from "./reference-server.ts";
import { sdkVersion } from "./sdk-version.ts";

const GATE_MS = 1;
const ms = (value: number) => value.toFixed(4);

function flag(name: string, fallback: number): number {
  const found = process.argv.find((arg) => arg.startsWith(`--${name}=`));
  return found === undefined ? fallback : Number(found.slice(name.length + 3));
}
const CALLS = flag("calls", 100_000);
const ROUNDS = flag("rounds", 40);
const LOOPBACK_CALLS = flag("loopback-calls", 2_000);

// Stand-in for the export queue: bounded, drops when full, never blocks.
const queue: Operation[] = [];
let recorded = 0;
const sink = (operation: Operation) => {
  recorded += 1;
  if (queue.length < 2048) {
    queue.push(operation);
  }
};

type Call = () => Promise<void>;

function quantile(sorted: Float64Array, q: number): number {
  return sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))] ?? Number.NaN;
}

interface Stats {
  p50: number;
  p99: number;
  mean: number;
}

function stats(samples: Float64Array): Stats {
  const sorted = samples.toSorted();
  let sum = 0;
  for (const value of sorted) {
    sum += value;
  }
  return { p50: quantile(sorted, 0.5), p99: quantile(sorted, 0.99), mean: sum / sorted.length };
}

async function measure(call: Call, samples: Float64Array, offset: number, count: number) {
  for (let i = 0; i < count; i += 1) {
    const start = performance.now();
    await call();
    samples[offset + i] = performance.now() - start;
  }
}

let gateFailed = false;

async function paired(path: string, gated: boolean, calls: number, variants: Record<string, Call>) {
  const names = Object.keys(variants);
  const perRound = Math.floor(calls / ROUNDS);
  const samples = new Map(names.map((name) => [name, new Float64Array(perRound * ROUNDS)]));
  for (const name of names) {
    const warm = new Float64Array(2000);
    await measure(variants[name] as Call, warm, 0, warm.length);
  }
  for (let round = 0; round < ROUNDS; round += 1) {
    // Rotate the order every round.
    for (let k = 0; k < names.length; k += 1) {
      const name = names[(round + k) % names.length] as string;
      await measure(
        variants[name] as Call,
        samples.get(name) as Float64Array,
        round * perRound,
        perRound,
      );
      queue.length = 0;
    }
  }
  const baseline = stats(samples.get("baseline") as Float64Array);
  console.log(`\n${path}: ${perRound * ROUNDS} calls per variant, ${ROUNDS} alternating rounds`);
  console.log(
    `  baseline       p50 ${ms(baseline.p50)} ms  p99 ${ms(baseline.p99)} ms  mean ${ms(baseline.mean)} ms`,
  );
  for (const name of names.filter((candidate) => candidate !== "baseline")) {
    const result = stats(samples.get(name) as Float64Array);
    const added = {
      p50: result.p50 - baseline.p50,
      p99: result.p99 - baseline.p99,
      mean: result.mean - baseline.mean,
    };
    const verdict = !gated ? "informational" : added.p99 <= GATE_MS ? "PASS" : "FAIL";
    if (verdict === "FAIL") {
      gateFailed = true;
    }
    console.log(
      `  ${name.padEnd(14)} p50 ${ms(result.p50)} ms  p99 ${ms(result.p99)} ms  mean ${ms(result.mean)} ms`,
    );
    console.log(
      `  ${"added".padEnd(14)} p50 ${ms(added.p50)} ms  p99 ${ms(added.p99)} ms  mean ${ms(added.mean)} ms  -> ${verdict} (gate: added p99 <= ${GATE_MS} ms)`,
    );
  }
}

const echo = { name: "echo", arguments: { text: "constant" } };

// Path 1: in-memory transport, long-lived connection (the stdio and session shape).
async function inMemoryCall(instrumented: boolean): Promise<Call> {
  const [clientSide, serverSide] = InMemoryTransport.createLinkedPair();
  const server = instrumented ? instrument(buildServer(), { sink }) : buildServer();
  await server.connect(serverSide);
  const client = new Client(
    { name: "bench", version: "0.0.0" },
    { versionNegotiation: { mode: "legacy" } },
  );
  await client.connect(clientSide);
  return async () => {
    await client.callTool(echo);
  };
}

// Path 2: the stateless handler called in process with a real modern request.
const handlers = {
  baseline: createMcpHandler(buildServer),
  tap: createMcpHandler(instrumentFactory(buildServer, { sink })),
  "tap+fetch-wrap": instrumentHandler(createMcpHandler(instrumentFactory(buildServer, { sink })), {
    sink,
  }),
};

async function captureModernRequest(): Promise<{ headers: Headers; body: string }> {
  let template: { headers: Headers; body: string } | undefined;
  const client = new Client(
    { name: "bench", version: "0.0.0" },
    { versionNegotiation: { mode: "auto" } },
  );
  await client.connect(
    new StreamableHTTPClientTransport(new URL("http://127.0.0.1/mcp"), {
      fetch: async (url, init) => {
        const request = new Request(url, init);
        const body = request.method === "POST" ? await request.clone().text() : "";
        if (body.includes('"tools/call"') && body.includes(PROTOCOL_VERSION_META_KEY)) {
          template = { headers: new Headers(request.headers), body };
        }
        return handlers.baseline.fetch(request);
      },
    }),
  );
  await client.callTool(echo);
  await client.close();
  if (template === undefined) {
    throw new Error("no modern tools/call request captured");
  }
  return template;
}

function handlerCall(
  handler: { fetch: (request: Request) => Promise<Response> },
  template: { headers: Headers; body: string },
): Call {
  return async () => {
    const response = await handler.fetch(
      new Request("http://127.0.0.1/mcp", {
        method: "POST",
        headers: template.headers,
        body: template.body,
      }),
    );
    const text = await response.text();
    if (response.status !== 200 || !text.includes("constant")) {
      throw new Error(`unexpected response ${response.status}: ${text.slice(0, 200)}`);
    }
  };
}

// Path 3: the same handler behind a loopback socket. Informational: socket jitter on a desktop OS
// is larger than the effect being measured.
async function loopbackCall(handler: { fetch: (request: Request) => Promise<Response> }): Promise<{
  call: Call;
  close: () => Promise<void>;
}> {
  const server = await host((request) => handler.fetch(request));
  const client = new Client(
    { name: "bench", version: "0.0.0" },
    { versionNegotiation: { mode: "auto" } },
  );
  await client.connect(new StreamableHTTPClientTransport(new URL(server.url)));
  return {
    call: async () => {
      await client.callTool(echo);
    },
    close: async () => {
      await client.close();
      await server.close();
    },
  };
}

console.log(
  `@modelcontextprotocol/server ${sdkVersion}, node ${process.version}, ${process.platform} ${process.arch}`,
);

await paired("in-memory transport (legacy era, long-lived connection)", true, CALLS, {
  baseline: await inMemoryCall(false),
  tap: await inMemoryCall(true),
});

const template = await captureModernRequest();
await paired("stateless handler in process (modern era, server per request)", true, CALLS, {
  baseline: handlerCall(handlers.baseline, template),
  tap: handlerCall(handlers.tap, template),
  "tap+fetch-wrap": handlerCall(handlers["tap+fetch-wrap"], template),
});

const loopBaseline = await loopbackCall(handlers.baseline);
const loopWrapped = await loopbackCall(handlers["tap+fetch-wrap"]);
await paired("stateless handler over loopback HTTP (modern era)", false, LOOPBACK_CALLS, {
  baseline: loopBaseline.call,
  "tap+fetch-wrap": loopWrapped.call,
});
await loopBaseline.close();
await loopWrapped.close();

console.log(`\noperations recorded by the sink: ${recorded}`);
console.log(gateFailed ? "LATENCY GATE FAILED" : "LATENCY GATE PASSED");
process.exit(gateFailed ? 1 : 0);
