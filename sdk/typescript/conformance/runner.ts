// The TypeScript SDK's runner for the shared conformance suite (docs/testing/sdk-conformance.md, runner contract
// v1). The driver writes a run spec to stdin; agent cases call the SDK in this process, MCP cases play the steps
// against a fresh reference server over stdio or streamable HTTP and print one `response` event per completed send.

import { type ChildProcessWithoutNullStreams, spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import {
  flush,
  type ObservationType,
  type ObservationUpdate,
  observe,
  shutdown,
  withContext,
} from "../src/index.ts";
import { initFromSpec, type RunSpec, startHttp } from "./server.ts";

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Message = { [key: string]: Json };

interface Completed {
  message: Message | null;
  status: number | null;
  sessionId: string | null;
  notifications: number;
}

function emit(step: string, completed: Completed): void {
  process.stdout.write(
    `${JSON.stringify({
      event: "response",
      step,
      http_status: completed.status,
      session_id: completed.sessionId,
      message: completed.message,
      notifications: completed.notifications,
    })}\n`,
  );
}

async function readStdin(): Promise<string> {
  let text = "";
  for await (const chunk of process.stdin) {
    text += String(chunk);
  }
  return text;
}

// ---- Agent cases -------------------------------------------------------------------------------------------

interface AgentNode {
  name: string;
  type?: ObservationType;
  children?: AgentNode[];
  trace_name?: string;
  context?: {
    session_id?: string;
    user_id?: string;
    tags?: string[];
    metadata?: Record<string, string | number | boolean>;
  };
  input?: unknown;
  output?: unknown;
  model?: string;
  provider?: string;
  input_messages?: unknown[];
  output_messages?: unknown[];
  finish_reasons?: string[];
  usage?: Record<string, number>;
  cost_usd?: Record<string, string | number>;
}

const USAGE: Record<string, keyof NonNullable<ObservationUpdate["usage"]>> = {
  input_tokens: "input",
  output_tokens: "output",
  "cache_read.input_tokens": "cacheRead",
  "cache_creation.input_tokens": "cacheWrite",
  "reasoning.output_tokens": "reasoning",
};

function updateOf(node: AgentNode): ObservationUpdate {
  const update: ObservationUpdate = {};
  if (node.trace_name !== undefined) {
    update.traceName = node.trace_name;
  }
  if (node.output !== undefined) {
    update.output = node.output;
  }
  if (node.model !== undefined) {
    update.model = node.model;
  }
  if (node.provider !== undefined) {
    update.provider = node.provider;
  }
  if (node.input_messages !== undefined) {
    update.inputMessages = node.input_messages;
  }
  if (node.output_messages !== undefined) {
    update.outputMessages = node.output_messages;
  }
  if (node.finish_reasons !== undefined) {
    update.finishReasons = node.finish_reasons;
  }
  if (node.usage !== undefined) {
    update.usage = {};
    for (const [key, count] of Object.entries(node.usage)) {
      const field = USAGE[key];
      if (field === undefined) {
        throw new Error(`usage key ${key} has no SDK field`);
      }
      update.usage[field] = count;
    }
  }
  if (node.cost_usd !== undefined) {
    update.cost = node.cost_usd;
  }
  return update;
}

async function observeNode(node: AgentNode): Promise<void> {
  const run = (): Promise<void> =>
    observe(
      node.name,
      {
        ...(node.type !== undefined && { type: node.type }),
        ...(node.input !== undefined && { input: node.input }),
      },
      async (observation) => {
        observation.update(updateOf(node));
        for (const child of node.children ?? []) {
          await observeNode(child);
        }
      },
    );
  const context = node.context;
  if (context === undefined) {
    return run();
  }
  return withContext(
    {
      ...(context.session_id !== undefined && { sessionId: context.session_id }),
      ...(context.user_id !== undefined && { userId: context.user_id }),
      ...(context.tags !== undefined && { tags: context.tags }),
      ...(context.metadata !== undefined && { metadata: context.metadata }),
    },
    run,
  );
}

async function runAgent(spec: RunSpec): Promise<void> {
  initFromSpec(spec);
  let shut = false;
  for (const step of spec.steps) {
    if ("observe" in step) {
      await observeNode(step.observe as AgentNode);
    } else if ("flush" in step) {
      await flush();
    } else if ("checkpoint" in step) {
      const response = await fetch(spec.checkpoint_url, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ name: step.checkpoint }),
      });
      if (response.status !== 204) {
        throw new Error(`checkpoint ${String(step.checkpoint)} answered ${response.status}`);
      }
    } else if ("shutdown" in step) {
      await shutdown();
      shut = true;
    } else {
      throw new Error(`unknown agent step ${JSON.stringify(step)}`);
    }
  }
  if (!shut) {
    await shutdown();
  }
}

// ---- MCP cases ---------------------------------------------------------------------------------------------

/** One way to exchange messages with the reference server. */
interface Channel {
  /** Sends a message and resolves when it completed: with its response, or at once for a notification. */
  send(step: string, message: Message, headers: Record<string, string>): Promise<Completed>;
  /** HTTP only: aborts the in-flight exchange of a step. */
  close(step: string): void;
  /** Ends the case, letting the SDK flush. */
  end(): Promise<void>;
}

function isRequest(message: Message): boolean {
  return "method" in message && "id" in message;
}

/** Replaces every `{"$from": "<step>/<pointer>"}` with the value in that step's response (RFC 6901). */
function resolve(value: Json, responses: Map<string, Message | null>): Json {
  if (Array.isArray(value)) {
    return value.map((item) => resolve(item, responses));
  }
  if (value === null || typeof value !== "object") {
    return value;
  }
  const from = value.$from;
  if (Object.keys(value).length === 1 && typeof from === "string") {
    const slash = from.indexOf("/");
    const step = from.slice(0, slash);
    let target: Json = responses.get(step) ?? null;
    for (const raw of from.slice(slash + 1).split("/")) {
      const token = raw.replaceAll("~1", "/").replaceAll("~0", "~");
      target =
        target !== null && typeof target === "object" && !Array.isArray(target)
          ? (target[token] ?? null)
          : Array.isArray(target)
            ? (target[Number(token)] ?? null)
            : null;
    }
    return target;
  }
  const out: { [key: string]: Json } = {};
  for (const [key, inner] of Object.entries(value)) {
    out[key] = resolve(inner, responses);
  }
  return out;
}

function stdioChannel(spec: RunSpec): Channel {
  const child: ChildProcessWithoutNullStreams = spawn(
    process.execPath,
    [fileURLToPath(new URL("./server.ts", import.meta.url)), "--stdio"],
    { env: { ...process.env, CRUCIBLE_CONFORMANCE_SPEC: JSON.stringify(spec) } },
  );
  child.stderr.pipe(process.stderr);
  const pending = new Map<Json, { done: (message: Message) => void; notifications: number }>();
  createInterface({ input: child.stdout }).on("line", (line) => {
    let message: Message;
    try {
      message = JSON.parse(line) as Message;
    } catch {
      return;
    }
    const id = message.id;
    const waiter = id === undefined ? undefined : pending.get(id);
    if (waiter !== undefined && ("result" in message || "error" in message)) {
      pending.delete(id ?? null);
      waiter.done(message);
      return;
    }
    for (const other of pending.values()) {
      other.notifications += 1;
    }
  });
  const exited = new Promise<void>((done) => child.on("exit", () => done()));
  return {
    send: (_step, message) => {
      child.stdin.write(`${JSON.stringify(message)}\n`);
      if (!isRequest(message)) {
        return Promise.resolve({ message: null, status: null, sessionId: null, notifications: 0 });
      }
      return new Promise((done) => {
        const waiter = {
          notifications: 0,
          done: (response: Message) =>
            done({
              message: response,
              status: null,
              sessionId: null,
              notifications: waiter.notifications,
            }),
        };
        pending.set(message.id ?? null, waiter);
      });
    },
    close: () => {
      throw new Error("close is an HTTP step");
    },
    end: async () => {
      child.stdin.end();
      await exited;
    },
  };
}

async function httpChannel(spec: RunSpec): Promise<Channel> {
  initFromSpec(spec);
  const server = await startHttp(spec);
  const aborts = new Map<string, AbortController>();
  let session: string | null = null;
  return {
    send: async (step, message, headers) => {
      const abort = new AbortController();
      aborts.set(step, abort);
      const response = await fetch(server.url, {
        method: "POST",
        signal: abort.signal,
        headers: {
          "content-type": "application/json",
          accept: "application/json, text/event-stream",
          ...headers,
          ...(session !== null && { "mcp-session-id": session }),
        },
        body: JSON.stringify(message),
      });
      const sessionId = response.headers.get("mcp-session-id");
      if (message.method === "initialize" && sessionId !== null) {
        session = sessionId;
      }
      const completed: Completed = {
        message: null,
        status: response.status,
        sessionId,
        notifications: 0,
      };
      if (!isRequest(message)) {
        await response.body?.cancel();
        return completed;
      }
      if (!(response.headers.get("content-type") ?? "").includes("text/event-stream")) {
        completed.message = (await response.json()) as Message;
        return completed;
      }
      // An event stream: each data line holds one JSON-RPC message; the one with our id is the response.
      if (response.body === null) {
        return completed;
      }
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";
      while (completed.message === null) {
        const { done, value } = await reader.read();
        if (done) {
          break;
        }
        buffer += decoder.decode(value, { stream: true });
        let newline = buffer.indexOf("\n");
        while (newline !== -1 && completed.message === null) {
          const line = buffer.slice(0, newline).replace(/\r$/, "");
          buffer = buffer.slice(newline + 1);
          newline = buffer.indexOf("\n");
          if (!line.startsWith("data:")) {
            continue;
          }
          const data = JSON.parse(line.slice(5).trim()) as Message;
          if (data.id === message.id && ("result" in data || "error" in data)) {
            completed.message = data;
          } else {
            completed.notifications += 1;
          }
        }
      }
      void reader.cancel().catch(() => undefined);
      return completed;
    },
    close: (step) => {
      aborts.get(step)?.abort();
    },
    end: async () => {
      for (const abort of aborts.values()) {
        abort.abort();
      }
      await server.close();
      await shutdown();
    },
  };
}

async function runMcp(spec: RunSpec): Promise<void> {
  const channel = spec.transport === "stdio" ? stdioChannel(spec) : await httpChannel(spec);
  const responses = new Map<string, Message | null>();
  const inFlight = new Map<string, Promise<void>>();
  for (const step of spec.steps) {
    const id = String(step.step);
    if ("send" in step) {
      const message = resolve(step.send as Json, responses) as Message;
      const headers = (step.headers ?? {}) as Record<string, string>;
      const exchange = channel.send(id, message, headers).then((completed) => {
        responses.set(id, completed.message);
        emit(id, completed);
      });
      if (step.wait === false) {
        // A cancelled request may never complete; it is never awaited.
        inFlight.set(
          id,
          exchange.catch(() => undefined),
        );
      } else {
        await exchange;
      }
    } else if ("await" in step) {
      await inFlight.get(String(step.await));
    } else if ("sleep_ms" in step) {
      await sleep(Number(step.sleep_ms));
    } else if ("close" in step) {
      channel.close(String(step.close));
    } else {
      throw new Error(`unknown MCP step ${JSON.stringify(step)}`);
    }
  }
  await channel.end();
}

const spec = JSON.parse(await readStdin()) as RunSpec;
try {
  await (spec.kind === "agent" ? runAgent(spec) : runMcp(spec));
  process.exit(0);
} catch (error) {
  process.stderr.write(
    `runner: ${error instanceof Error ? (error.stack ?? error.message) : String(error)}\n`,
  );
  process.exit(1);
}
