// The payload viewer's model: what the worker does with a field's bytes. Pure, so it runs the same in the
// worker and in tests: decode, parse, index the JSON tree by pointer, and read a message list.

export type JsonKind = "object" | "array" | "string" | "number" | "boolean" | "null";

export interface JsonNode {
  // A JSON pointer from the document root: unique, and how the worker finds the node again.
  id: string;
  // The member name or index under its parent.
  key: string;
  kind: JsonKind;
  // The value on one line: a scalar (cut to PREVIEW_CHARS), or how many members or items it holds.
  preview: string;
  // How many children an object or array has; 0 for a scalar.
  size: number;
}

export type MessagePart =
  | { kind: "text"; text: string }
  | { kind: "tool_call"; name: string; text: string }
  | { kind: "tool_result"; text: string }
  | { kind: "data"; text: string };

export interface Message {
  id: string;
  role: string;
  parts: MessagePart[];
}

export type Parsed =
  | { kind: "json"; root: JsonNode; top: JsonNode[]; messages: Message[] | null }
  | { kind: "text"; text: string; reason: "not_json" | "empty" };

const PREVIEW_CHARS = 200;

// Bytes that are not UTF-8 become U+FFFD; a character cut at the end of a truncated field does too.
export function decode(bytes: Uint8Array): string {
  return new TextDecoder("utf-8", { fatal: false }).decode(bytes);
}

function kindOf(value: unknown): JsonKind {
  if (value === null) return "null";
  if (Array.isArray(value)) return "array";
  switch (typeof value) {
    case "object":
      return "object";
    case "string":
      return "string";
    case "number":
      return "number";
    default:
      return "boolean";
  }
}

function plural(count: number, one: string, many: string): string {
  return `${count.toLocaleString("en-US")} ${count === 1 ? one : many}`;
}

function cut(text: string): string {
  return text.length > PREVIEW_CHARS ? `${text.slice(0, PREVIEW_CHARS)}…` : text;
}

function node(id: string, key: string, value: unknown): JsonNode {
  const kind = kindOf(value);
  if (kind === "array") {
    const size = (value as unknown[]).length;
    return { id, key, kind, preview: `[${plural(size, "item", "items")}]`, size };
  }
  if (kind === "object") {
    const size = Object.keys(value as object).length;
    return { id, key, kind, preview: `{${plural(size, "key", "keys")}}`, size };
  }
  const preview = kind === "string" ? cut(JSON.stringify(value)) : String(value);
  return { id, key, kind, preview, size: 0 };
}

function escape(segment: string): string {
  return segment.replaceAll("~", "~0").replaceAll("/", "~1");
}

function unescape(segment: string): string {
  return segment.replaceAll("~1", "/").replaceAll("~0", "~");
}

// The value at a JSON pointer, or undefined when the pointer names nothing.
export function resolve(root: unknown, pointer: string): unknown {
  if (pointer === "") return root;
  let value: unknown = root;
  for (const raw of pointer.slice(1).split("/")) {
    const segment = unescape(raw);
    if (Array.isArray(value)) {
      const index = Number(segment);
      if (!Number.isInteger(index) || index < 0 || index >= value.length) return undefined;
      value = value[index];
    } else if (value !== null && typeof value === "object" && Object.hasOwn(value, segment)) {
      value = (value as Record<string, unknown>)[segment];
    } else {
      return undefined;
    }
  }
  return value;
}

// The children of the node at `pointer`, in document order.
export function children(root: unknown, pointer: string): JsonNode[] {
  const value = resolve(root, pointer);
  if (Array.isArray(value)) {
    return value.map((item, index) => node(`${pointer}/${index}`, String(index), item));
  }
  if (value !== null && typeof value === "object") {
    return Object.entries(value).map(([key, item]) => node(`${pointer}/${escape(key)}`, key, item));
  }
  return [];
}

function asText(value: unknown): string {
  if (typeof value === "string") return value;
  return JSON.stringify(value, null, 2) ?? String(value);
}

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function toolCall(call: unknown): MessagePart {
  const item = record(call) ?? {};
  const fn = record(item.function);
  const name = item.name ?? fn?.name;
  const args = item.arguments ?? fn?.arguments ?? "";
  return { kind: "tool_call", name: typeof name === "string" ? name : "tool", text: asText(args) };
}

// One part of a GenAI message (`type` text, tool_call, tool_call_response), or of a content list.
function part(value: unknown): MessagePart {
  const item = record(value);
  if (item === null) return { kind: "text", text: asText(value) };
  switch (item.type) {
    case "text":
      return { kind: "text", text: asText(item.content ?? item.text ?? "") };
    case "tool_call":
      return toolCall(item);
    case "tool_call_response":
      return { kind: "tool_result", text: asText(item.response ?? item.result ?? "") };
    default:
      return { kind: "data", text: asText(value) };
  }
}

function message(value: unknown, index: number): Message | null {
  const item = record(value);
  if (item === null || typeof item.role !== "string") return null;
  const parts: MessagePart[] = [];
  if (Array.isArray(item.parts)) parts.push(...item.parts.map(part));
  if (typeof item.content === "string") parts.push({ kind: "text", text: item.content });
  else if (Array.isArray(item.content)) parts.push(...item.content.map(part));
  else if (item.content !== undefined && item.content !== null) parts.push(part(item.content));
  if (Array.isArray(item.tool_calls)) parts.push(...item.tool_calls.map(toolCall));
  if (parts.length === 0) {
    const { role: _role, ...rest } = item;
    if (Object.keys(rest).length > 0) parts.push({ kind: "data", text: asText(rest) });
  }
  return { id: `message-${index}`, role: item.role, parts };
}

// A list of chat messages, each an object with a `role`: GenAI messages carry `parts`, messages rebuilt
// from numbered attributes carry `content` and `tool_calls`. Anything else is not a message list.
export function messagesOf(value: unknown): Message[] | null {
  if (!Array.isArray(value) || value.length === 0) return null;
  const messages = value.map(message);
  return messages.every((m) => m !== null) ? (messages as Message[]) : null;
}

export class PayloadDocument {
  readonly value: unknown;
  readonly parsed: Parsed;

  constructor(bytes: Uint8Array) {
    const source = decode(bytes);
    let value: unknown;
    let json = false;
    if (source.trim() !== "") {
      try {
        value = JSON.parse(source);
        json = true;
      } catch {
        json = false;
      }
    }
    this.value = value;
    this.parsed = json
      ? {
          kind: "json",
          root: node("", "", value),
          top: children(value, ""),
          messages: messagesOf(value),
        }
      : { kind: "text", text: source, reason: source.trim() === "" ? "empty" : "not_json" };
  }

  children(pointer: string): JsonNode[] {
    return children(this.value, pointer);
  }
}
