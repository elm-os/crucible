#!/usr/bin/env node
// Crucible text policy: the naming rule and the no-AI-attribution rule.
//
// Usage:
//   node tools/policy/policy.mjs files [--staged]   scan tracked (or staged) text files
//   node tools/policy/policy.mjs commit-msg <file>   check a commit message file (git hook)
//   node tools/policy/policy.mjs text [<string>]     check a string, or stdin (PR title/body)
//   node tools/policy/policy.mjs branch <name>       check a branch name
//   node tools/policy/policy.mjs commits <range>     check every commit message in a git range
//   node tools/policy/policy.mjs selftest            run the must-flag / must-pass fixtures
//
// Exit code 0 means clean; 1 means at least one violation; 2 means a usage error.

import { execFileSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..", "..");
const rules = JSON.parse(readFileSync(join(root, "policy", "naming-rules.json"), "utf8"));

const attribution = rules.attribution_patterns.map((p) => new RegExp(p, "im"));
const vendorRe = new RegExp(`\\b(${rules.vendors.map(escapeRe).join("|")})\\b`, "i");
const provenance = new Set(rules.provenance_words.flatMap((w) => w.split(" ")));
const vendors = new Set(rules.vendors);

function escapeRe(s) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function normalize(text) {
  return text
    .toLowerCase()
    .replace(/[*_`~#>|]/g, " ")
    .replace(/[-‐-―]/g, " ")
    .replace(/\s+/g, " ")
    .trim();
}

function tokens(text) {
  return normalize(text).split(" ").filter(Boolean);
}

function hasProvenanceNear(toks, start, len, window) {
  const lo = Math.max(0, start - window);
  const hi = Math.min(toks.length, start + len + window);
  for (let i = lo; i < hi; i++) {
    const t = toks[i].replace(/[^a-z0-9.]/g, "");
    if (provenance.has(t) || vendors.has(t) || vendors.has(t.split(".")[0])) return true;
  }
  return false;
}

function findPhrase(toks, phrase) {
  const p = phrase.split(" ");
  const hits = [];
  outer: for (let i = 0; i + p.length <= toks.length; i++) {
    for (let j = 0; j < p.length; j++) {
      if (toks[i + j].replace(/[^a-z0-9]/g, "") !== p[j]) continue outer;
    }
    hits.push(i);
  }
  return hits;
}

// Returns a list of {rule, detail} for one chunk of text.
function checkNaming(text) {
  const out = [];
  const toks = tokens(text);
  for (const phrase of rules.always_flag) {
    if (findPhrase(toks, phrase).length) out.push({ rule: "naming", detail: `phrase "${phrase}"` });
  }
  for (const phrase of rules.context_flag) {
    for (const at of findPhrase(toks, phrase)) {
      if (hasProvenanceNear(toks, at, phrase.split(" ").length, rules.context_window_tokens)) {
        out.push({
          rule: "naming",
          detail: `phrase "${phrase}" next to a provenance or vendor word`,
        });
        break;
      }
    }
  }
  return out;
}

function checkAttribution(text) {
  return attribution.some((re) => re.test(text))
    ? [{ rule: "attribution", detail: "AI attribution line or trailer" }]
    : [];
}

function isExempt(path) {
  return rules.exempt_paths.some((p) => (p.endsWith("/") ? path.startsWith(p) : path === p));
}

function vendorAllowed(path) {
  return rules.vendor_allowed_paths.some((p) =>
    p.endsWith("/") ? path.startsWith(p) : path === p,
  );
}

function scanText(label, text, { vendorCheck = false } = {}) {
  const violations = [];
  const lines = text.split(/\r?\n/);
  const seen = new Set();
  const report = (lineNo, v) => {
    const key = `${lineNo}:${v.rule}:${v.detail}`;
    if (seen.has(key)) return;
    seen.add(key);
    const excerpt = (lines[lineNo - 1] ?? "").trim().slice(0, 140);
    violations.push(`${label}:${lineNo}: [${v.rule}] ${v.detail}: ${excerpt}`);
  };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (line.includes(rules.waiver_marker)) continue;
    const pair =
      i + 1 < lines.length && !lines[i + 1].includes(rules.waiver_marker)
        ? `${line} ${lines[i + 1]}`
        : line;
    for (const v of checkNaming(line)) report(i + 1, v);
    if (pair !== line) {
      const single = new Set(checkNaming(line).map((v) => v.detail));
      const next = new Set(checkNaming(lines[i + 1]).map((v) => v.detail));
      for (const v of checkNaming(pair))
        if (!single.has(v.detail) && !next.has(v.detail)) report(i + 1, v);
    }
    for (const v of checkAttribution(line)) report(i + 1, v);
    if (vendorCheck && vendorRe.test(line)) {
      report(i + 1, {
        rule: "vendor-confinement",
        detail: `vendor name outside ${rules.vendor_allowed_paths.join(", ")}`,
      });
    }
  }
  return violations;
}

function git(args) {
  return execFileSync("git", args, { cwd: root, encoding: "utf8", maxBuffer: 256 * 1024 * 1024 });
}

function looksBinary(buf) {
  const n = Math.min(buf.length, 8000);
  for (let i = 0; i < n; i++) if (buf[i] === 0) return true;
  return false;
}

function scanFiles(staged) {
  const list = staged
    ? git(["diff", "--cached", "--name-only", "--diff-filter=ACMR", "-z"])
    : git(["ls-files", "-z", "--cached", "--others", "--exclude-standard"]);
  const files = list.split("\0").filter(Boolean);
  const violations = [];
  for (const f of files) {
    const path = f.replace(/\\/g, "/");
    if (isExempt(path)) continue;
    const full = join(root, path);
    if (!existsSync(full)) continue;
    const buf = readFileSync(full);
    if (looksBinary(buf)) continue;
    violations.push(...scanText(path, buf.toString("utf8"), { vendorCheck: !vendorAllowed(path) }));
    if (vendorRe.test(path) && !vendorAllowed(path)) {
      violations.push(
        `${path}:0: [vendor-confinement] vendor name in a path outside the allowed directories`,
      );
    }
  }
  return violations;
}

function commitMessageBody(text) {
  // Drop git's comment lines and the scissors section from a commit message file.
  const cut = text.split(/^# -+ >8 -+$/m)[0];
  return cut
    .split(/\r?\n/)
    .filter((l) => !l.startsWith("#"))
    .join("\n");
}

function selftest() {
  const flag = readFileSync(join(here, "fixtures", "must-flag.txt"), "utf8")
    .split(/\r?\n/)
    .filter((l) => l.trim());
  const pass = readFileSync(join(here, "fixtures", "must-pass.txt"), "utf8")
    .split(/\r?\n/)
    .filter((l) => l.trim());
  const failures = [];
  for (const l of flag)
    if (scanText("fixture", l, { vendorCheck: true }).length === 0)
      failures.push(`should flag: ${l}`);
  for (const l of pass) if (scanText("fixture", l).length !== 0) failures.push(`should pass: ${l}`);
  if (failures.length) {
    console.error(failures.join("\n"));
    return 1;
  }
  console.log(`policy selftest ok (${flag.length} must-flag, ${pass.length} must-pass)`);
  return 0;
}

function finish(violations, what) {
  if (violations.length) {
    console.error(violations.join("\n"));
    console.error(
      `\npolicy: ${violations.length} violation(s) in ${what}. See AGENTS.md "Naming rule" and "No AI attribution".`,
    );
    return 1;
  }
  console.log(`policy: ${what} clean`);
  return 0;
}

function readStdin() {
  try {
    return readFileSync(0, "utf8");
  } catch {
    return "";
  }
}

function main(argv) {
  const [cmd, ...rest] = argv;
  switch (cmd) {
    case "files":
      return finish(
        scanFiles(rest.includes("--staged")),
        rest.includes("--staged") ? "staged files" : "tracked files",
      );
    case "commit-msg": {
      if (!rest[0]) return usage();
      const body = commitMessageBody(readFileSync(rest[0], "utf8"));
      return finish(scanText("commit message", body), "commit message");
    }
    case "text": {
      const text = rest.length ? rest.join(" ") : readStdin();
      return finish(scanText("text", text), "text");
    }
    case "branch": {
      if (!rest[0]) return usage();
      return finish(scanText("branch", rest[0].replace(/[/_-]/g, " ")), "branch name");
    }
    case "commits": {
      if (!rest[0]) return usage();
      const shas = git(["rev-list", rest[0]]).split(/\s+/).filter(Boolean);
      const violations = [];
      for (const sha of shas) {
        const msg = git(["log", "-1", "--format=%B", sha]);
        violations.push(...scanText(`commit ${sha.slice(0, 12)}`, msg));
      }
      return finish(violations, `${shas.length} commit message(s)`);
    }
    case "selftest":
      return selftest();
    default:
      return usage();
  }
}

function usage() {
  console.error(
    "usage: policy.mjs files [--staged] | commit-msg <file> | text [<s>] | branch <name> | commits <range> | selftest",
  );
  return 2;
}

process.exit(main(process.argv.slice(2)));
