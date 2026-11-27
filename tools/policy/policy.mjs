#!/usr/bin/env node
// Crucible text policy: the naming rule, the no-AI-attribution rule and the commit identity rule.
//
// Usage:
//   node tools/policy/policy.mjs files [--staged]   scan tracked (or staged) text files
//   node tools/policy/policy.mjs commit-msg <file>   check a commit message file (git hook)
//   node tools/policy/policy.mjs text [<string>]     check a string, or stdin (PR title/body)
//   node tools/policy/policy.mjs branch <name>       check a branch name
//   node tools/policy/policy.mjs commits <range>     check every commit in a git range (message, author,
//                                                    committer); a bare revision is that one commit
//   node tools/policy/policy.mjs selftest            run the fixtures and the planted-commit test
//
// Exit code 0 means clean; 1 means at least one violation; 2 means a usage error.

import { execFileSync } from "node:child_process";
import { readFileSync, existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..", "..");
const rules = JSON.parse(readFileSync(join(root, "policy", "naming-rules.json"), "utf8"));

const attribution = rules.attribution_patterns.map((p) => new RegExp(p, "im"));
const coAuthor = rules.co_author_patterns.map((p) => new RegExp(p, "im"));
const [, ownerName, ownerEmail] = /^(.+) <(.+)>$/.exec(rules.commit_authors[0]);
const identityFix =
  `git -c user.name="${ownerName}" -c user.email="${ownerEmail}" commit --amend --no-edit --reset-author ` +
  `for the newest commit, or the same -c flags with rebase --exec "git commit --amend --no-edit ` +
  `--reset-author" origin/main for older ones, then push with --force-with-lease`;
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

// GitHub credits every account named in a co-author trailer as a contributor of the squash merge, whatever
// else the line says, so the waiver marker does not apply here.
function checkCoAuthors(label, text) {
  return text
    .split(/\r?\n/)
    .flatMap((line, i) =>
      coAuthor.some((re) => re.test(line))
        ? [
            `${label}:${i + 1}: [co-author] ${line.trim().slice(0, 140)}: no commit message or pull ` +
              `request body may carry a co-author trailer, because GitHub credits the named account as a ` +
              `contributor when the pull request is squash-merged; fix: delete the line (git commit --amend ` +
              `for the newest commit, a rebase for older ones, or edit the pull request description)`,
          ]
        : [],
    );
}

function checkIdentity(label, author, committer) {
  if (rules.commit_authors.includes(author) && rules.commit_committers.includes(committer))
    return [];
  return [
    `${label}: [identity] author "${author}", committer "${committer}": the author must be ` +
      `${rules.commit_authors.join(" or ")} and the committer ${rules.commit_committers.join(" or ")}, ` +
      `because GitHub credits every commit author of a pull request as a contributor when it is ` +
      `squash-merged; fix: ${identityFix}`,
  ];
}

// A commit message, or a pull request title and body.
function checkMessage(label, text) {
  return [...scanText(label, text), ...checkCoAuthors(label, text)];
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

function git(args, cwd = root) {
  return execFileSync("git", args, { cwd, encoding: "utf8", maxBuffer: 256 * 1024 * 1024 });
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

// A range (a..b) checks the commits it selects. A bare revision checks that one commit, never its history,
// so a commit already on main is not checked again by a later push.
function checkCommits(range, cwd = root) {
  const walk = range.includes("..") ? [range] : ["--no-walk", range];
  const shas = git(["rev-list", ...walk], cwd)
    .split(/\s+/)
    .filter(Boolean);
  const violations = [];
  for (const sha of shas) {
    // %an and %ae, never %aN and %aE: a .mailmap in the checked tree must not rename an identity.
    const [author, committer, msg] = git(
      ["log", "-1", "--format=%an <%ae>%x00%cn <%ce>%x00%B", sha],
      cwd,
    ).split("\0");
    const label = `commit ${sha.slice(0, 12)}`;
    violations.push(...checkIdentity(label, author, committer), ...checkMessage(label, msg));
  }
  return { shas, violations };
}

function fixture(name) {
  return readFileSync(join(here, "fixtures", name), "utf8")
    .split(/\r?\n/)
    .filter((l) => l.trim());
}

// Plants a commit that breaks both commit rules between two clean commits in a scratch repository, then
// checks that the range reports it, and that the clean tip alone does not re-check its history.
function plantedCommits() {
  const dir = mkdtempSync(join(tmpdir(), "crucible-policy-"));
  try {
    const run = (args, env = {}) =>
      execFileSync("git", args, {
        cwd: dir,
        encoding: "utf8",
        env: { ...process.env, ...env },
        stdio: "pipe",
      }).trim();
    run(["init", "-q"]);
    const tree = run(["write-tree"]);
    const commit = (parent, author, committer, ...paragraphs) => {
      const [, an, ae] = /^(.+) <(.+)>$/.exec(author);
      const [, cn, ce] = /^(.+) <(.+)>$/.exec(committer);
      const env = {
        GIT_AUTHOR_NAME: an,
        GIT_AUTHOR_EMAIL: ae,
        GIT_COMMITTER_NAME: cn,
        GIT_COMMITTER_EMAIL: ce,
      };
      const args = ["commit-tree", tree, ...(parent ? ["-p", parent] : [])];
      return run([...args, ...paragraphs.flatMap((p) => ["-m", p])], env);
    };
    const owner = rules.commit_authors[0];
    const stranger = "Jane Doe <jane@example.com>";
    writeFileSync(join(dir, ".mailmap"), `${owner} ${stranger}\n`);
    const trailer = `Co-authored-by: ${stranger}`;
    const base = commit(null, owner, owner, "chore: base");
    const bad = commit(base, stranger, stranger, "chore: checkpoint", trailer);
    const tip = commit(bad, owner, "GitHub <noreply@github.com>", "fix: a clean change");
    const found = checkCommits(`${base}..${tip}`, dir).violations;
    const badLabel = `commit ${bad.slice(0, 12)}`;
    const failures = [];
    if (!found.some((v) => v.startsWith(`${badLabel}: [identity] author "${stranger}"`)))
      failures.push("planted: the stranger's author and committer were not reported");
    if (!found.some((v) => v.startsWith(`${badLabel}:3: [co-author] ${trailer}`)))
      failures.push("planted: the co-author trailer was not reported");
    if (found.some((v) => v.startsWith(`commit ${tip.slice(0, 12)}`)))
      failures.push("planted: the clean commit was reported");
    if (checkCommits(tip, dir).violations.length)
      failures.push("planted: a bare revision checked the history below it");
    return failures;
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

function selftest() {
  const flag = fixture("must-flag.txt");
  const pass = fixture("must-pass.txt");
  const idFlag = fixture("identity-must-flag.txt");
  const idPass = fixture("identity-must-pass.txt");
  const failures = [];
  for (const l of flag) {
    const found = [
      ...scanText("fixture", l, { vendorCheck: true }),
      ...checkCoAuthors("fixture", l),
    ];
    if (found.length === 0) failures.push(`should flag: ${l}`);
  }
  for (const l of pass)
    if (checkMessage("fixture", l).length !== 0) failures.push(`should pass: ${l}`);
  for (const l of idFlag)
    if (checkIdentity("fixture", ...l.split(" | ")).length === 0)
      failures.push(`should flag identity: ${l}`);
  for (const l of idPass)
    if (checkIdentity("fixture", ...l.split(" | ")).length !== 0)
      failures.push(`should pass identity: ${l}`);
  failures.push(...plantedCommits());
  if (failures.length) {
    console.error(failures.join("\n"));
    return 1;
  }
  console.log(
    `policy selftest ok (${flag.length} must-flag, ${pass.length} must-pass, ${idFlag.length} identity ` +
      `must-flag, ${idPass.length} identity must-pass, planted commits caught)`,
  );
  return 0;
}

function finish(violations, what) {
  if (violations.length) {
    console.error(violations.join("\n"));
    console.error(
      `\npolicy: ${violations.length} violation(s) in ${what}. See AGENTS.md "Naming rule" and "No AI attribution", and policy/naming-rules.json for the allowed commit identities.`,
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
      return finish(checkMessage("commit message", body), "commit message");
    }
    case "text": {
      const text = rest.length ? rest.join(" ") : readStdin();
      return finish(checkMessage("text", text), "text");
    }
    case "branch": {
      if (!rest[0]) return usage();
      return finish(scanText("branch", rest[0].replace(/[/_-]/g, " ")), "branch name");
    }
    case "commits": {
      if (!rest[0]) return usage();
      const { shas, violations } = checkCommits(rest[0]);
      return finish(violations, `${shas.length} commit(s)`);
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
