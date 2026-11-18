import { afterEach, describe, expect, test, vi } from "vitest";
import { createPayloadParser } from "./payload-parser";

afterEach(() => vi.unstubAllGlobals());

const bytes = (text: string) => new TextEncoder().encode(text);

// A worker that fails as soon as it is asked anything, as one the page could not load does.
class FailingWorker extends EventTarget {
  postMessage() {
    queueMicrotask(() => this.dispatchEvent(new Event("error")));
  }
}

describe("the parser", () => {
  test("without a Worker, documents are parsed and opened on the page", async () => {
    const parser = createPayloadParser();
    const parsed = await parser.parse("doc", bytes('{"a": {"b": 1}}'));
    expect(parsed.kind).toBe("json");
    expect((await parser.children("doc", "/a")).map((n) => n.key)).toEqual(["b"]);
    await expect(parser.children("gone", "")).rejects.toThrow();
  });

  test("a worker that fails hands every later call to the page, so nothing waits forever", async () => {
    vi.stubGlobal("Worker", FailingWorker);
    const parser = createPayloadParser();
    const parsed = await parser.parse("doc", bytes("[1, 2]"));
    expect(parsed.kind).toBe("json");
    expect((await parser.children("doc", "")).map((n) => n.preview)).toEqual(["1", "2"]);
    const again = await parser.parse("other", bytes("plain"));
    expect(again).toMatchObject({ kind: "text", text: "plain" });
  });

  test("a worker the page may not start leaves parsing to the page", async () => {
    vi.stubGlobal("Worker", function RefusedWorker() {
      throw new DOMException("refused by policy", "SecurityError");
    });
    const parsed = await createPayloadParser().parse("doc", bytes('"x"'));
    expect(parsed).toMatchObject({ kind: "json", top: [] });
  });
});
