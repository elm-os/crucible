// The installed version of the MCP server package (its package.json is not an exported subpath).

import { readFileSync } from "node:fs";

const manifest = new URL("../package.json", import.meta.resolve("@modelcontextprotocol/server"));
export const sdkVersion = (JSON.parse(readFileSync(manifest, "utf8")) as { version: string })
  .version;
