// Exit criterion 2: the spike sources never reach into private members of the MCP SDK and never
// escape the type checker. `tsc --noEmit` (strict) enforces TypeScript `private`; this scan bans
// the ways around it. Usage: node scripts/check-private.mjs [selftest]

import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

// `_meta` is a field of the JSON-RPC wire format, not a member of an SDK object.
const WIRE_FIELDS = new Set(["_meta"]);

const RULES = [
  { name: "underscore member access", pattern: /\??\.\s*(_\w+)/g, allow: WIRE_FIELDS },
  { name: "underscore bracket access", pattern: /\[\s*["'`](_\w+)["'`]\s*\]/g, allow: WIRE_FIELDS },
  { name: "any type", pattern: /(\bas\s+any\b|:\s*any\b|<any>|\bany\[\])/g },
  { name: "double cast", pattern: /\bas\s+unknown\s+as\b/g },
  { name: "type checker suppression", pattern: /@ts-(ignore|expect-error|nocheck)/g },
  { name: "prototype patching", pattern: /\.prototype\b|\b__proto__\b|setPrototypeOf/g },
];

export function scan(text) {
  const findings = [];
  text.split("\n").forEach((line, index) => {
    for (const rule of RULES) {
      for (const match of line.matchAll(rule.pattern)) {
        if (rule.allow?.has(match[1])) {
          continue;
        }
        findings.push({ line: index + 1, rule: rule.name, text: line.trim() });
      }
    }
  });
  return findings;
}

function selftest() {
  const planted = [
    ["server._requestHandlers.get('tools/call')", "underscore member access"],
    ["server?._transport", "underscore member access"],
    ["server['_requestHandlers']", "underscore bracket access"],
    ["const x = server as any;", "any type"],
    ["function f(x: any) {}", "any type"],
    ["const y = t as unknown as Hidden;", "double cast"],
    ["// @ts-expect-error private", "type checker suppression"],
    ["McpServer.prototype.connect = patched;", "prototype patching"],
  ];
  let ok = true;
  for (const [line, rule] of planted) {
    const caught = scan(line).some((finding) => finding.rule === rule);
    console.log(`${caught ? "caught" : "MISSED"}  ${rule}: ${line}`);
    ok &&= caught;
  }
  const clean = scan("const meta = fields(params?._meta); body.params._meta['x'] = 1;");
  console.log(`${clean.length === 0 ? "clean " : "FLAGGED"}  wire field _meta is allowed`);
  return ok && clean.length === 0;
}

const src = fileURLToPath(new URL("../src", import.meta.url));
if (process.argv[2] === "selftest") {
  process.exit(selftest() ? 0 : 1);
}
let total = 0;
const files = readdirSync(src).filter((name) => name.endsWith(".ts"));
for (const name of files) {
  for (const finding of scan(readFileSync(join(src, name), "utf8"))) {
    total += 1;
    console.log(`src/${name}:${finding.line}  ${finding.rule}  ${finding.text}`);
  }
}
console.log(`${files.length} files scanned, ${total} findings`);
process.exit(total === 0 ? 0 : 1);
