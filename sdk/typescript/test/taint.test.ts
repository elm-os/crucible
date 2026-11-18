// Planted secrets in every place contract 9.1 says a sender must never emit them from: none reaches the
// collector, even with content capture on for every tool.

import { StreamableHTTPClientTransport } from "@modelcontextprotocol/client";
import { createMcpHandler, PROTOCOL_VERSION_META_KEY } from "@modelcontextprotocol/server";
import { afterAll, beforeAll, expect, test } from "vitest";
import { flush, init, shutdown } from "../src/index.ts";
import { instrument, instrumentHandler } from "../src/mcp/index.ts";
import { type Collector, startCollector } from "./support/collector.ts";
import { host } from "./support/http-host.ts";
import { buildServer, REQUEST_STATE } from "./support/reference-server.ts";
import { fromTemplate, modernTemplate, newClient } from "./support/scenarios.ts";

const MARKERS = {
  bearer: "TAINTBEARER0123456789",
  cookie: "TAINTCOOKIE",
  param: "TAINTPARAM",
  baggage: "TAINTBAGGAGE",
  token: "TAINTTOKEN",
  password: "TAINTPASSWORD",
  key: "csk_0TAINTKEYabcdefghijklmnopqrstuvwxyzABCDEFG",
  email: "taint.person@example.com",
  state: REQUEST_STATE,
};

let collector: Collector;

beforeAll(async () => {
  collector = await startCollector();
  init({ endpoint: collector.url, secretKey: collector.key, flushIntervalMs: 10 });
});

afterAll(async () => {
  await shutdown();
  await collector.close();
});

test("no planted secret appears in any exported byte", async () => {
  const handler = instrumentHandler(
    createMcpHandler(
      instrument(buildServer, {
        capture: [{ tool: "*" }],
        identify: () => ({ tenant: "acme" }),
        handleParams: { secret: "note" },
      }),
    ),
  );
  const planted = (headers: Headers): void => {
    headers.set("authorization", `Bearer ${MARKERS.bearer}`);
    headers.set("cookie", `session=${MARKERS.cookie}`);
    headers.set("mcp-param-note", MARKERS.param);
  };
  const authInfo = { token: MARKERS.token, clientId: "client-1", scopes: [] };
  const template = await modernTemplate();

  // A captured call whose arguments and result carry secrets, with every planted header and _meta key.
  const call = await handler.fetch(
    fromTemplate(template, (body, headers) => {
      planted(headers);
      const params = body.params as Record<string, unknown> & { _meta: Record<string, unknown> };
      params.name = "secret";
      params.arguments = {
        password: MARKERS.password,
        note: `key ${MARKERS.key} mail ${MARKERS.email}`,
      };
      params._meta.baggage = `user=${MARKERS.baggage}`;
      headers.set("mcp-name", "secret");
    }),
    { authInfo },
  );
  expect(call.status).toBe(200);
  expect(await call.text()).toContain(MARKERS.email);

  // An exchange the fetch wrap records itself (an unsupported revision), with the same planted values.
  const edge = await handler.fetch(
    fromTemplate(template, (body, headers) => {
      planted(headers);
      const params = body.params as { _meta: Record<string, unknown> };
      params._meta[PROTOCOL_VERSION_META_KEY] = "2099-01-01";
      params._meta.baggage = `user=${MARKERS.baggage}`;
      headers.set("mcp-protocol-version", "2099-01-01");
    }),
    { authInfo },
  );
  expect(edge.status).toBe(400);

  // A multi-round call whose requestState goes out and comes back.
  const server = await host((request) => handler.fetch(request, { authInfo }));
  const client = newClient("auto");
  await client.connect(
    new StreamableHTTPClientTransport(new URL(server.url), {
      requestInit: {
        headers: { authorization: `Bearer ${MARKERS.bearer}`, cookie: `s=${MARKERS.cookie}` },
      },
    }),
  );
  await client.callTool({ name: "needs_input", arguments: {} });
  await client.close();
  await server.close();
  await handler.close();
  await flush();

  const tools = collector.spans.map((span) => span.attributes["gen_ai.tool.name"]);
  expect(tools).toContain("secret");
  expect(tools).toContain("needs_input");
  expect(
    collector.spans.some((span) => span.attributes["http.response.status_code"] === 400n),
  ).toBe(true);
  const exported = Buffer.concat(collector.requests.map((request) => request.body));
  for (const [name, marker] of Object.entries(MARKERS)) {
    expect(exported.includes(marker), `${name} leaked`).toBe(false);
  }
  // The scrubbed forms are what was captured.
  expect(exported.includes("[redacted:sensitive_key]")).toBe(true);
  expect(exported.includes("[redacted:crucible_key]")).toBe(true);
  expect(exported.includes("[redacted:email]")).toBe(true);
});
