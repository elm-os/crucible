// Child process for the stdio scenarios: node stdio-server.ts <connect|serve> <collector url> <secret key>.
// It exports to the collector, flushes when its stdin closes, and then exits by itself.

import { StdioServerTransport, serveStdio } from "@modelcontextprotocol/server/stdio";
import { init, shutdown } from "../../src/index.ts";
import { instrument } from "../../src/mcp/index.ts";
import { buildServer, SERVER_NAME } from "./reference-server.ts";

const [mode, endpoint, secretKey] = process.argv.slice(2);
if ((mode !== "connect" && mode !== "serve") || endpoint === undefined || secretKey === undefined) {
  throw new Error("usage: stdio-server.ts <connect|serve> <collector url> <secret key>");
}
init({ endpoint, secretKey, serviceName: SERVER_NAME, flushIntervalMs: 20 });
process.stdin.on("end", () => void shutdown());

if (mode === "connect") {
  await instrument(buildServer()).connect(new StdioServerTransport());
} else {
  serveStdio(instrument(buildServer));
}
