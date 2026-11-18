// Parses payloads off the main thread. The page sends a field's bytes once and then asks for the children of
// the nodes the reader opens, so a large document never crosses to the main thread whole.
import { type JsonNode, type Parsed, PayloadDocument } from "./payload-model";

export type WorkerRequest =
  | { type: "parse"; call: number; doc: string; bytes: ArrayBuffer }
  | { type: "children"; call: number; doc: string; pointer: string }
  | { type: "drop"; doc: string };

export type WorkerReply =
  | { type: "parsed"; call: number; parsed: Parsed }
  | { type: "children"; call: number; nodes: JsonNode[] }
  | { type: "failed"; call: number };

const documents = new Map<string, PayloadDocument>();

// The worker scope's postMessage takes no target origin, unlike the window's in the DOM types.
const scope = globalThis as unknown as { postMessage(message: WorkerReply): void };
const reply = (message: WorkerReply) => scope.postMessage(message);

addEventListener("message", (event: MessageEvent<WorkerRequest>) => {
  const request = event.data;
  if (request.type === "drop") {
    documents.delete(request.doc);
    return;
  }
  try {
    if (request.type === "parse") {
      const document = new PayloadDocument(new Uint8Array(request.bytes));
      documents.set(request.doc, document);
      reply({ type: "parsed", call: request.call, parsed: document.parsed });
    } else {
      const nodes = documents.get(request.doc)?.children(request.pointer) ?? [];
      reply({ type: "children", call: request.call, nodes });
    }
  } catch {
    reply({ type: "failed", call: request.call });
  }
});
