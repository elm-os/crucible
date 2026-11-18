// The page's side of the payload worker: one worker for the page, documents named by the caller. Where no
// Worker exists (tests, very old browsers), or once the worker has failed, the same model runs on the main
// thread.
import { type JsonNode, type Parsed, PayloadDocument } from "./payload-model";
import type { WorkerReply, WorkerRequest } from "./payload.worker";

export interface PayloadParser {
  parse(doc: string, bytes: Uint8Array): Promise<Parsed>;
  children(doc: string, pointer: string): Promise<JsonNode[]>;
  drop(doc: string): void;
}

class InlineParser implements PayloadParser {
  private readonly documents = new Map<string, PayloadDocument>();

  async parse(doc: string, bytes: Uint8Array): Promise<Parsed> {
    const document = new PayloadDocument(bytes);
    this.documents.set(doc, document);
    return document.parsed;
  }

  async children(doc: string, pointer: string): Promise<JsonNode[]> {
    const document = this.documents.get(doc);
    if (!document) throw new Error("the payload is no longer loaded");
    return document.children(pointer);
  }

  drop(doc: string): void {
    this.documents.delete(doc);
  }
}

type Pending = { resolve: (reply: WorkerReply) => void; reject: (error: Error) => void };

class WorkerParser implements PayloadParser {
  private readonly worker: Worker;
  private readonly pending = new Map<number, Pending>();
  private readonly fallback = new InlineParser();
  private broken = false;
  private next = 1;

  constructor() {
    this.worker = new Worker(new URL("./payload.worker.ts", import.meta.url));
    this.worker.addEventListener("message", (event: MessageEvent<WorkerReply>) => {
      const waiting = this.pending.get(event.data.call);
      this.pending.delete(event.data.call);
      waiting?.resolve(event.data);
    });
    // A worker that fails, for example because it could not load, answers nothing more.
    this.worker.addEventListener("error", () => {
      this.broken = true;
      for (const waiting of this.pending.values())
        waiting.reject(new Error("payload worker failed"));
      this.pending.clear();
    });
  }

  private call(request: WorkerRequest & { call: number }, transfer: Transferable[] = []) {
    if (this.broken) return Promise.reject(new Error("payload worker failed"));
    return new Promise<WorkerReply>((resolve, reject) => {
      this.pending.set(request.call, { resolve, reject });
      this.worker.postMessage(request, transfer);
    });
  }

  async parse(doc: string, bytes: Uint8Array): Promise<Parsed> {
    try {
      // A copy is transferred, so the caller's bytes stay usable for the fallback.
      const buffer = bytes.slice().buffer;
      const message = { type: "parse" as const, call: this.next++, doc, bytes: buffer };
      const reply = await this.call(message, [buffer]);
      if (reply.type === "parsed") return reply.parsed;
    } catch {
      // The worker failed: the page parses the field itself.
    }
    return this.fallback.parse(doc, bytes);
  }

  async children(doc: string, pointer: string): Promise<JsonNode[]> {
    if (this.broken) return this.fallback.children(doc, pointer);
    const reply = await this.call({ type: "children", call: this.next++, doc, pointer });
    if (reply.type !== "children") return this.fallback.children(doc, pointer);
    return reply.nodes;
  }

  drop(doc: string): void {
    this.fallback.drop(doc);
    if (!this.broken) this.worker.postMessage({ type: "drop", doc } satisfies WorkerRequest);
  }
}

export function createPayloadParser(): PayloadParser {
  if (typeof Worker === "undefined") return new InlineParser();
  try {
    return new WorkerParser();
  } catch {
    // A page whose policy refuses the worker parses on the main thread.
    return new InlineParser();
  }
}

let shared: PayloadParser | null = null;

export function payloadParser(): PayloadParser {
  shared ??= createPayloadParser();
  return shared;
}
