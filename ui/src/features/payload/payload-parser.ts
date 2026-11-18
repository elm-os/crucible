// The page's side of the payload worker: one worker for the page, documents named by the caller. Where no
// Worker exists (tests, very old browsers) the same model runs on the main thread.
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
    return this.documents.get(doc)?.children(pointer) ?? [];
  }

  drop(doc: string): void {
    this.documents.delete(doc);
  }
}

type Pending = { resolve: (reply: WorkerReply) => void; reject: (error: Error) => void };

class WorkerParser implements PayloadParser {
  private readonly worker: Worker;
  private readonly pending = new Map<number, Pending>();
  private next = 1;

  constructor() {
    this.worker = new Worker(new URL("./payload.worker.ts", import.meta.url));
    this.worker.addEventListener("message", (event: MessageEvent<WorkerReply>) => {
      const waiting = this.pending.get(event.data.call);
      this.pending.delete(event.data.call);
      waiting?.resolve(event.data);
    });
    this.worker.addEventListener("error", () => {
      for (const waiting of this.pending.values())
        waiting.reject(new Error("payload worker failed"));
      this.pending.clear();
    });
  }

  private call(request: WorkerRequest & { call: number }, transfer: Transferable[] = []) {
    return new Promise<WorkerReply>((resolve, reject) => {
      this.pending.set(request.call, { resolve, reject });
      this.worker.postMessage(request, transfer);
    });
  }

  async parse(doc: string, bytes: Uint8Array): Promise<Parsed> {
    // A copy is transferred, so the caller's bytes stay usable.
    const buffer = bytes.slice().buffer;
    const reply = await this.call({ type: "parse", call: this.next++, doc, bytes: buffer }, [
      buffer,
    ]);
    if (reply.type !== "parsed") throw new Error("the payload could not be parsed");
    return reply.parsed;
  }

  async children(doc: string, pointer: string): Promise<JsonNode[]> {
    const reply = await this.call({ type: "children", call: this.next++, doc, pointer });
    if (reply.type !== "children") throw new Error("the payload could not be read");
    return reply.nodes;
  }

  drop(doc: string): void {
    this.worker.postMessage({ type: "drop", doc } satisfies WorkerRequest);
  }
}

let shared: PayloadParser | null = null;

export function payloadParser(): PayloadParser {
  shared ??= typeof Worker === "undefined" ? new InlineParser() : new WorkerParser();
  return shared;
}
