// Child process for the stdio scenarios. Usage: node stdio-server.ts <connect|serve> <ops-file>
// Recorded operations are appended to <ops-file> as JSON lines.

import { appendFileSync } from "node:fs";
import { StdioServerTransport, serveStdio } from "@modelcontextprotocol/server/stdio";
import { instrument, instrumentFactory, type Operation } from "./instrument.ts";
import { buildServer } from "./reference-server.ts";

const [mode, opsFile] = process.argv.slice(2);
if ((mode !== "connect" && mode !== "serve") || opsFile === undefined) {
  throw new Error("usage: stdio-server.ts <connect|serve> <ops-file>");
}
const sink = (operation: Operation) => appendFileSync(opsFile, `${JSON.stringify(operation)}\n`);

if (mode === "connect") {
  await instrument(buildServer(), { sink }).connect(new StdioServerTransport());
} else {
  serveStdio(instrumentFactory(buildServer, { sink }));
}
