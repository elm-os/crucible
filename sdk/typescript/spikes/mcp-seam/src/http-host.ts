// Minimal node:http host for a fetch-shaped handler (the spike avoids extra dependencies).

import { createServer, type Server as HttpServer } from "node:http";
import type { AddressInfo } from "node:net";

export type FetchHandler = (request: Request) => Promise<Response>;

export interface Host {
  url: string;
  close: () => Promise<void>;
}

export async function host(handler: FetchHandler): Promise<Host> {
  const server: HttpServer = createServer(async (req, res) => {
    const chunks: Buffer[] = [];
    for await (const chunk of req) {
      chunks.push(chunk as Buffer);
    }
    const headers = new Headers();
    for (const [name, value] of Object.entries(req.headers)) {
      if (typeof value === "string") {
        headers.set(name, value);
      }
    }
    const abort = new AbortController();
    res.on("close", () => abort.abort());
    const method = req.method ?? "GET";
    const hasBody = method !== "GET" && method !== "HEAD" && method !== "DELETE";
    const response = await handler(
      new Request(`http://127.0.0.1${req.url ?? "/"}`, {
        method,
        headers,
        signal: abort.signal,
        ...(hasBody && { body: Buffer.concat(chunks) }),
      }),
    );
    res.writeHead(response.status, Object.fromEntries(response.headers));
    if (response.body === null) {
      res.end();
      return;
    }
    const reader = response.body.getReader();
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) {
          break;
        }
        res.write(value);
      }
    } catch {
      // The exchange was torn down while streaming.
    }
    res.end();
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return {
    url: `http://127.0.0.1:${port}/mcp`,
    close: () =>
      new Promise<void>((resolve) => {
        server.closeAllConnections();
        server.close(() => resolve());
      }),
  };
}
