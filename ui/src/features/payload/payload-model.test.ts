import { describe, expect, test } from "vitest";
import { children, messagesOf, PayloadDocument, resolve } from "./payload-model";

const bytes = (text: string) => new TextEncoder().encode(text);

describe("the document", () => {
  test("a JSON object lists its members at the top, each with a one-line preview", () => {
    const doc = new PayloadDocument(
      bytes('{"a/b": {"c~d": [1, "two", null]}, "n": 3.5, "t": true}'),
    );
    expect(doc.parsed.kind).toBe("json");
    if (doc.parsed.kind !== "json") return;
    expect(doc.parsed.root).toMatchObject({ id: "", kind: "object", size: 3, preview: "{3 keys}" });
    expect(doc.parsed.top.map((n) => [n.id, n.key, n.kind, n.preview])).toEqual([
      ["/a~1b", "a/b", "object", "{1 key}"],
      ["/n", "n", "number", "3.5"],
      ["/t", "t", "boolean", "true"],
    ]);
    // Pointers escape `/` and `~`, and children are found by them again.
    expect(doc.children("/a~1b").map((n) => n.id)).toEqual(["/a~1b/c~0d"]);
    expect(doc.children("/a~1b/c~0d").map((n) => [n.key, n.kind, n.preview])).toEqual([
      ["0", "number", "1"],
      ["1", "string", '"two"'],
      ["2", "null", "null"],
    ]);
    expect(doc.children("/n")).toEqual([]);
    expect(doc.children("/missing")).toEqual([]);
  });

  test("long strings are cut in their preview, never in the document", () => {
    const long = "x".repeat(500);
    const doc = new PayloadDocument(bytes(JSON.stringify([long])));
    if (doc.parsed.kind !== "json") throw new Error("json expected");
    expect(doc.parsed.top[0]?.preview.length).toBeLessThan(220);
    expect(doc.parsed.top[0]?.preview.endsWith("…")).toBe(true);
    expect(resolve(doc.value, "/0")).toBe(long);
  });

  test("text, JSON cut at the size limit and an empty field are text", () => {
    expect(new PayloadDocument(bytes("plain words")).parsed).toEqual({
      kind: "text",
      text: "plain words",
      reason: "not_json",
    });
    expect(new PayloadDocument(bytes('{"cut": "at the li')).parsed.kind).toBe("text");
    expect(new PayloadDocument(bytes("  ")).parsed).toMatchObject({
      kind: "text",
      reason: "empty",
    });
    // A character cut at the end decodes as a replacement, not an error.
    const cut = bytes("hé").subarray(0, 2);
    expect(new PayloadDocument(cut).parsed).toEqual({
      kind: "text",
      text: "h�",
      reason: "not_json",
    });
  });

  test("resolve and children ignore what a pointer cannot name", () => {
    const value = { list: [0, 1] };
    expect(resolve(value, "/list/7")).toBeUndefined();
    expect(resolve(value, "/list/-1")).toBeUndefined();
    expect(resolve(value, "/list/x")).toBeUndefined();
    expect(children(value, "/list/0")).toEqual([]);
  });
});

describe("messages", () => {
  test("GenAI messages read their parts: text, tool calls and tool responses", () => {
    const messages = messagesOf([
      { role: "system", parts: [{ type: "text", content: "Be brief." }] },
      {
        role: "assistant",
        parts: [{ type: "tool_call", id: "c1", name: "find_city", arguments: { q: "x" } }],
      },
      { role: "tool", parts: [{ type: "tool_call_response", id: "c1", response: "Lisbon" }] },
      { role: "user", parts: [{ type: "image", uri: "x" }] },
    ]);
    expect(messages?.map((m) => [m.role, m.parts.map((p) => p.kind)])).toEqual([
      ["system", ["text"]],
      ["assistant", ["tool_call"]],
      ["tool", ["tool_result"]],
      ["user", ["data"]],
    ]);
    expect(messages?.[1]?.parts[0]).toEqual({
      kind: "tool_call",
      name: "find_city",
      text: '{\n  "q": "x"\n}',
    });
  });

  test("messages rebuilt from numbered attributes read their content and tool calls", () => {
    const messages = messagesOf([
      { role: "user", content: "What is the forecast?" },
      {
        role: "assistant",
        content: null,
        tool_calls: [{ id: "call_a1", function: { arguments: "{}", name: "find_city" } }],
      },
      { role: "tool", tool_call_id: "call_a1", content: "Lisbon" },
      { finish_reason: "stop", role: "assistant", content: [{ type: "text", text: "Sunny." }] },
    ]);
    expect(messages?.map((m) => m.parts)).toEqual([
      [{ kind: "text", text: "What is the forecast?" }],
      [{ kind: "tool_call", name: "find_city", text: "{}" }],
      [{ kind: "text", text: "Lisbon" }],
      [{ kind: "text", text: "Sunny." }],
    ]);
  });

  test("anything that is not a list of objects with a role is not a message list", () => {
    expect(messagesOf([])).toBeNull();
    expect(messagesOf({ role: "user" })).toBeNull();
    expect(messagesOf([{ role: "user", content: "hi" }, { content: "no role" }])).toBeNull();
    expect(messagesOf("text")).toBeNull();
  });
});
