#!/usr/bin/env node
// Serves the static export in ui/out the way the binary does (contract 12.1): exact files, `/x/` as
// `x/index.html`, slashless routes as the page directly, unknown API paths as a problem 404, and
// everything else as 404.html. No dependencies.
//
// Usage: node e2e/serve.mjs [--port 4173]

import { createReadStream, statSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, extname, join, normalize, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

const { values: args } = parseArgs({ options: { port: { type: "string", default: "4173" } } });
const root = join(dirname(fileURLToPath(import.meta.url)), "..", "out");

const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json",
  ".txt": "text/plain; charset=utf-8",
  ".woff2": "font/woff2",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".ico": "image/x-icon",
  ".map": "application/json",
};

function file(path) {
  try {
    return statSync(path).isFile() ? path : null;
  } catch {
    return null;
  }
}

function resolve(pathname) {
  const relative = normalize(decodeURIComponent(pathname)).replace(/^[/\\]+/, "");
  const target = join(root, relative);
  if (target !== root && !target.startsWith(root + sep)) return null;
  if (pathname.endsWith("/")) return file(join(target, "index.html"));
  return file(target) ?? file(join(target, "index.html"));
}

function send(response, status, path) {
  response.writeHead(status, {
    "content-type": TYPES[extname(path)] ?? "application/octet-stream",
    "cache-control": extname(path) === ".html" ? "no-cache" : "max-age=60",
  });
  createReadStream(path).pipe(response);
}

const server = createServer((request, response) => {
  const { pathname } = new URL(request.url ?? "/", "http://localhost");
  if (pathname.startsWith("/api/")) {
    response.writeHead(404, { "content-type": "application/problem+json" });
    response.end(JSON.stringify({ code: "not_found", status: 404, request_id: "serve-404" }));
    return;
  }
  let path = null;
  try {
    path = resolve(pathname);
  } catch {
    // A malformed escape in the path is answered like any unknown path.
  }
  if (path) send(response, 200, path);
  else send(response, 404, join(root, "404.html"));
});

server.listen(Number(args.port), "127.0.0.1", () => {
  console.log(`Serving ${root} at http://127.0.0.1:${args.port}`);
});
