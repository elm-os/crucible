#!/usr/bin/env node
// Drives headless Chrome over the DevTools protocol against a running `crucible serve`: opens the traces
// page, connects through the temporary Connect dialog, and checks that the traces table shows the expected
// traces. Prints one JSON object with what it saw; exits 1 with the page text when a step times out.
//
// Usage: node tools/e2e/ui-traces.mjs --chrome <binary> --url http://127.0.0.1:8080 --project <uuid>
//          --key <csk_...> --expect <trace name> [--expect ...] [--screenshot <file.png>]
//
// No dependencies: Node 24 has a global WebSocket.

import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { parseArgs } from "node:util";

const { values: args } = parseArgs({
  options: {
    chrome: { type: "string" },
    url: { type: "string" },
    project: { type: "string" },
    key: { type: "string" },
    expect: { type: "string", multiple: true },
    screenshot: { type: "string" },
  },
});
for (const name of ["chrome", "url", "project", "key", "expect"]) {
  if (!args[name]) throw new Error(`--${name} is required`);
}

const STEP_TIMEOUT_MS = 30_000;
const profile = mkdtempSync(join(tmpdir(), "crucible-e2e-chrome-"));
const chrome = spawn(
  args.chrome,
  [
    "--headless=new",
    "--remote-debugging-port=0",
    `--user-data-dir=${profile}`,
    "--no-sandbox",
    "--disable-gpu",
    "--disable-dev-shm-usage",
    "--no-first-run",
    "--window-size=1280,900",
    "about:blank",
  ],
  { stdio: ["ignore", "ignore", "pipe"] },
);

function cleanUp() {
  chrome.kill();
  try {
    rmSync(profile, { recursive: true, force: true, maxRetries: 5 });
  } catch {
    // Chrome may still hold files of its profile for a moment; the directory is in the temp folder.
  }
}

const browserUrl = await new Promise((resolve, reject) => {
  let log = "";
  const timer = setTimeout(() => reject(new Error(`Chrome did not start:\n${log}`)), STEP_TIMEOUT_MS);
  chrome.stderr.on("data", (chunk) => {
    log += chunk;
    const found = /DevTools listening on (ws:\/\/\S+)/.exec(log);
    if (found) {
      clearTimeout(timer);
      resolve(found[1]);
    }
  });
  chrome.on("exit", (code) => reject(new Error(`Chrome exited with ${code}:\n${log}`)));
});

const socket = new WebSocket(browserUrl);
await new Promise((resolve, reject) => {
  socket.addEventListener("open", resolve, { once: true });
  socket.addEventListener("error", () => reject(new Error("the DevTools socket failed")), { once: true });
});
let nextId = 0;
const waiting = new Map();
const consoleErrors = [];
socket.addEventListener("message", (event) => {
  const message = JSON.parse(event.data);
  if (message.method === "Runtime.exceptionThrown") {
    consoleErrors.push(message.params.exceptionDetails.text);
  }
  const reply = waiting.get(message.id);
  if (!reply) return;
  waiting.delete(message.id);
  if (message.error) reply.reject(new Error(`${reply.method}: ${message.error.message}`));
  else reply.resolve(message.result);
});

function send(method, params = {}, sessionId = undefined) {
  nextId += 1;
  const id = nextId;
  socket.send(JSON.stringify({ id, method, params, sessionId }));
  return new Promise((resolve, reject) => waiting.set(id, { resolve, reject, method }));
}

const { targetId } = await send("Target.createTarget", { url: "about:blank" });
const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
const page = (method, params) => send(method, params, sessionId);

async function evaluate(expression) {
  const { result, exceptionDetails } = await page("Runtime.evaluate", {
    expression,
    returnByValue: true,
    awaitPromise: true,
  });
  if (exceptionDetails) throw new Error(`page script failed: ${exceptionDetails.text}`);
  return result.value;
}

async function until(what, expression) {
  const deadline = Date.now() + STEP_TIMEOUT_MS;
  for (;;) {
    const value = await evaluate(expression);
    if (value) return value;
    if (Date.now() > deadline) {
      const text = await evaluate("document.body ? document.body.innerText : ''");
      throw new Error(`timed out waiting for ${what}. The page shows:\n${text}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
}

// Focuses the control with this label and types into it as a keyboard would.
async function type(label, text) {
  const focused = await evaluate(`(() => {
    const dialog = document.querySelector('[data-slot="dialog-content"]');
    const label = [...dialog.querySelectorAll("label")].find((l) => l.textContent.trim() === ${JSON.stringify(label)});
    const input = label && (label.control ?? document.getElementById(label.htmlFor));
    if (!input) return false;
    input.focus();
    return document.activeElement === input;
  })()`);
  if (!focused) throw new Error(`no field labelled ${label}`);
  await page("Input.insertText", { text });
}

const clickButton = (scope, name) => `(() => {
  const root = document.querySelector(${JSON.stringify(scope)});
  const button = root && [...root.querySelectorAll("button")].find((b) => b.textContent.trim() === ${JSON.stringify(name)});
  if (!button) return false;
  button.click();
  return true;
})()`;

const rowsExpression = `[...document.querySelectorAll("table tbody tr")].map((row) =>
  [...row.cells].map((cell) => cell.innerText.trim()))`;

try {
  await page("Page.enable");
  await page("Runtime.enable");
  await page("Page.navigate", { url: `${args.url}/traces/` });

  // Not connected: the section says so and offers the dialog.
  const before = await until(
    "the section to ask for a connection",
    `(() => {
      const card = document.querySelector('[data-error="unauthenticated"]');
      return card ? card.innerText : null;
    })()`,
  );
  await until("the Connect button in the header", clickButton("header", "Connect"));
  const dialog = await until(
    "the Connect dialog",
    `(() => {
      const dialog = document.querySelector('[data-slot="dialog-content"][data-open]');
      return dialog ? dialog.innerText : null;
    })()`,
  );
  await type("Project id", args.project);
  await type("Secret key", args.key);
  await until("the dialog's Connect button", clickButton('[data-slot="dialog-content"] form', "Connect"));

  await until(
    "the traces table to show every expected trace",
    `(() => {
      const names = ${rowsExpression}.map((cells) => cells[1]);
      return ${JSON.stringify(args.expect)}.every((name) => names.includes(name));
    })()`,
  );
  const rows = await evaluate(rowsExpression);
  const stored = await evaluate(`({
    sessionKey: sessionStorage.getItem("crucible.connection.key") !== null,
    localStorageHoldsKey: JSON.stringify({ ...localStorage }).includes(${JSON.stringify(args.key)}),
    header: document.querySelector("header").innerText,
    columns: [...document.querySelectorAll("table thead th")].map((th) => th.innerText.trim()),
  })`);
  if (!stored.sessionKey || stored.localStorageHoldsKey) {
    throw new Error("the key must be in sessionStorage and never in localStorage");
  }
  if (args.screenshot) {
    const { data } = await page("Page.captureScreenshot", { format: "png" });
    writeFileSync(args.screenshot, Buffer.from(data, "base64"));
  }
  console.log(
    JSON.stringify({
      before_connect: before.split("\n")[0],
      dialog_title: dialog.split("\n")[0],
      connected_header: stored.header.includes("Connected"),
      columns: stored.columns,
      rows,
      console_errors: consoleErrors,
      screenshot: args.screenshot ?? null,
    }),
  );
} catch (error) {
  console.error(String(error instanceof Error ? error.message : error));
  process.exitCode = 1;
} finally {
  socket.close();
  cleanUp();
}
