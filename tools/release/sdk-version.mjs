#!/usr/bin/env node
// The four SDKs and the docs' install commands carry the server's release version.
//
// Usage:
//   node tools/release/sdk-version.mjs set              write the workspace version (Cargo.toml) into every SDK file
//                                                       and every install reference in the docs
//   node tools/release/sdk-version.mjs check [version]  fail unless the workspace, every SDK file and every install
//                                                       reference in the docs hold the version (the workspace version
//                                                       when none is given); then print it, its Python form and whether
//                                                       it is a pre-release as name=value lines
//
// A version is X.Y.Z or a pre-release X.Y.Z-alpha.N, X.Y.Z-beta.N or X.Y.Z-rc.N. The Python files hold the Python
// form of it: 0.2.0rc1 for 0.2.0-rc.1. Exit code 0 means every file agrees; 1 means one does not; 2 is a usage error.
//
// An install reference is a place in README.md, sdk/*/README.md or docs/**/*.md that names a release: a download from
// its GitHub release, an SDK package's file name, a raw file at its tag, its image, the Rust git tag, the Go module
// version or tag, or the words "release of Crucible X.Y.Z" or "version of Crucible X.Y.Z". `keep` lists the references
// that name another version on purpose. docs/releasing.md and CHANGELOG.md are never changed.
//
// Tests: node --test tools/release/sdk-version.test.mjs

import { existsSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..");

// Every place that holds an SDK's version. The pattern's first group is the version, and it must match exactly once.
const manifest = /^ {2}"sdk_version": "([^"]*)",$/m;
export const places = [
  { file: "sdk/typescript/package.json", pattern: /^ {2}"version": "([^"]*)",$/m },
  { file: "sdk/typescript/src/setup.ts", pattern: /^export const SDK_VERSION = "([^"]*)";$/m },
  { file: "sdk/typescript/conformance/manifest.json", pattern: manifest },
  { file: "sdk/python/pyproject.toml", pattern: /^version = "([^"]*)"$/m, python: true },
  {
    file: "sdk/python/src/crucible_sdk/_version.py",
    pattern: /^__version__ = "([^"]*)"$/m,
    python: true,
  },
  {
    file: "sdk/python/uv.lock",
    pattern: /^name = "crucible-sdk"\nversion = "([^"]*)"$/m,
    python: true,
  },
  { file: "sdk/python/conformance/manifest.json", pattern: manifest, python: true },
  { file: "sdk/go/internal/core/client.go", pattern: /^const Version = "([^"]*)"$/m },
  { file: "sdk/go/conformance/manifest.json", pattern: manifest },
  {
    file: "sdk/rust/Cargo.toml",
    pattern: /^\[package\]\nname = "crucible-sdk"\nversion = "([^"]*)"$/m,
  },
  { file: "sdk/rust/Cargo.lock", pattern: /^name = "crucible-sdk"\nversion = "([^"]*)"$/m },
  { file: "sdk/rust/conformance/manifest.json", pattern: manifest },
];

// A version as the docs write it, with any pre-release, and the Python form of one.
const semver = String.raw`\d+\.\d+\.\d+(?:-[0-9A-Za-z]+(?:\.[0-9A-Za-z]+)*)?`;
const pep440 = String.raw`\d+\.\d+\.\d+(?:[a-z]+\d+)?`;

// Every kind of install reference. The pattern's first group is the version.
const references = [
  { pattern: String.raw`github\.com/elm-os/crucible/releases/download/v(${semver})/` },
  { pattern: String.raw`raw\.githubusercontent\.com/elm-os/crucible/v(${semver})/` },
  { pattern: String.raw`\belm-os-crucible-(${semver})\.tgz` },
  { pattern: String.raw`\bcrucible_sdk-(${pep440})-[\w.-]+\.whl`, python: true },
  { pattern: String.raw`\bcrucible_sdk-(${pep440})\.tar\.gz`, python: true },
  { pattern: String.raw`\bcrucible-sdk-(${semver})\.crate` },
  { pattern: String.raw`ghcr\.io/elm-os/crucible:(${semver})` },
  { pattern: String.raw`github\.com/elm-os/crucible(?:\.git)?".*?\btag = "v(${semver})"` },
  { pattern: String.raw`github\.com/elm-os/crucible/sdk/go@v(${semver})` },
  { pattern: String.raw`\bsdk/go/v(${semver})` },
  { pattern: String.raw`\b(?:release|version) of Crucible (${semver})` },
].map(({ pattern, python = false }) => ({ pattern: new RegExp(pattern, "gd"), python }));

// Install references that name another version on purpose, each a file and the reference as `check` prints it. `set`
// leaves them as they are and `check` accepts them.
const keep = [
  // A placeholder version, which the -replace flag on the same line points at a checkout.
  { file: "docs/guides/sdk-go.md", reference: "github.com/elm-os/crucible/sdk/go@v0.0.0" },
];

// The release steps show example versions of past releases, which stay as they are.
const unscanned = ["docs/releasing.md"];

function usage(message) {
  console.error(`${message}\nusage: node tools/release/sdk-version.mjs set | check [version]`);
  process.exit(2);
}

// The Python form of a version, or null when the version is not one this release process uses.
export function pythonForm(version) {
  const m = /^(\d+\.\d+\.\d+)(?:-(alpha|beta|rc)\.(\d+))?$/.exec(version);
  if (m === null) {
    return null;
  }
  const [, release, kind, number] = m;
  return kind === undefined
    ? release
    : `${release}${{ alpha: "a", beta: "b", rc: "rc" }[kind]}${number}`;
}

// The one match of the pattern in the text, with the indices of the version, or an error message.
function locate(text, pattern) {
  const matches = [...text.matchAll(new RegExp(pattern.source, `${pattern.flags}gd`))];
  if (matches.length !== 1) {
    return { error: `${matches.length} matches of ${pattern}, expected 1` };
  }
  const [start, end] = matches[0].indices[1];
  return { version: matches[0][1], start, end };
}

// The install references in one docs file that have to name the release, in the order they appear, each with its
// line, its text, its version, whether that is the Python form, and the indices of the version.
export function docReferences(file, text) {
  const found = [];
  for (const { pattern, python } of references) {
    for (const match of text.matchAll(pattern)) {
      if (keep.some((entry) => entry.file === file && entry.reference === match[0])) {
        continue;
      }
      const [start, end] = match.indices[1];
      const line = text.slice(0, start).split("\n").length;
      found.push({ line, reference: match[0], version: match[1], python, start, end });
    }
  }
  return found.toSorted((a, b) => a.start - b.start);
}

function read(file) {
  return readFileSync(join(root, file), "utf8");
}

// The docs that can hold install references, as paths from the root with forward slashes.
function docFiles() {
  const sdks = readdirSync(join(root, "sdk"), { withFileTypes: true })
    .filter((entry) => entry.isDirectory())
    .map((entry) => `sdk/${entry.name}/README.md`)
    .filter((file) => existsSync(join(root, file)));
  const docs = readdirSync(join(root, "docs"), { recursive: true })
    .map((name) => `docs/${name.replaceAll("\\", "/")}`)
    .filter((file) => file.endsWith(".md") && !unscanned.includes(file));
  return ["README.md", ...sdks, ...docs].toSorted();
}

function workspaceVersion() {
  const text = read("Cargo.toml");
  const table = text
    .split(/^(?=\[)/m)
    .find((section) => section.startsWith("[workspace.package]\n"));
  const found = /^version = "([^"]*)"$/m.exec(table ?? "");
  if (found === null) {
    console.error("Cargo.toml: no version in [workspace.package]");
    process.exit(1);
  }
  return found[1];
}

function expectedVersions(version) {
  const python = pythonForm(version);
  if (python === null) {
    usage(`${version} is not X.Y.Z, X.Y.Z-alpha.N, X.Y.Z-beta.N or X.Y.Z-rc.N`);
  }
  return { version, python };
}

function main() {
  const [command, argument, ...rest] = process.argv.slice(2);
  if (rest.length > 0 || (command === "set" && argument !== undefined)) {
    usage("too many arguments");
  }

  if (command === "set") {
    const { version, python } = expectedVersions(workspaceVersion());
    let failed = false;
    for (const place of places) {
      const text = read(place.file);
      const found = locate(text, place.pattern);
      const want = place.python ? python : version;
      if (found.error !== undefined) {
        console.error(`${place.file}: ${found.error}`);
        failed = true;
      } else if (found.version !== want) {
        writeFileSync(
          join(root, place.file),
          text.slice(0, found.start) + want + text.slice(found.end),
        );
        console.log(`${place.file}: ${found.version} -> ${want}`);
      }
    }
    for (const file of docFiles()) {
      const text = read(file);
      const stale = docReferences(file, text)
        .map((ref) => ({ ...ref, want: ref.python ? python : version }))
        .filter((ref) => ref.version !== ref.want);
      let updated = text;
      for (const ref of stale.toReversed()) {
        updated = updated.slice(0, ref.start) + ref.want + updated.slice(ref.end);
      }
      if (stale.length > 0) {
        writeFileSync(join(root, file), updated);
      }
      for (const ref of stale) {
        console.log(`${file}:${ref.line}: ${ref.version} -> ${ref.want}`);
      }
    }
    console.log(
      failed
        ? "some files were not updated"
        : `every SDK file and install reference in the docs holds ${version}`,
    );
    process.exit(failed ? 1 : 0);
  } else if (command === "check") {
    const workspace = workspaceVersion();
    const { version, python } = expectedVersions(argument ?? workspace);
    const problems = [];
    if (workspace !== version) {
      problems.push(`Cargo.toml: the workspace version is ${workspace}, expected ${version}`);
    }
    for (const place of places) {
      const found = locate(read(place.file), place.pattern);
      const want = place.python ? python : version;
      if (found.error !== undefined) {
        problems.push(`${place.file}: ${found.error}`);
      } else if (found.version !== want) {
        problems.push(`${place.file}: holds ${found.version}, expected ${want}`);
      }
    }
    const docs = docFiles().flatMap((file) =>
      docReferences(file, read(file)).map((ref) => ({
        file,
        ...ref,
        want: ref.python ? python : version,
      })),
    );
    const stale = docs.filter((ref) => ref.version !== ref.want);
    for (const ref of stale) {
      problems.push(
        `${ref.file}:${ref.line}: ${ref.reference} names ${ref.version}, expected ${ref.want}`,
      );
    }
    if (problems.length > 0) {
      console.error(problems.join("\n"));
      console.error(
        "Set the workspace version in Cargo.toml, then run `node tools/release/sdk-version.mjs set` (docs/releasing.md).",
      );
      if (stale.length > 0) {
        console.error(
          "A docs line that names another version on purpose goes on the keep list in tools/release/sdk-version.mjs.",
        );
      }
      process.exit(1);
    }
    console.error(
      `the workspace, the ${places.length} SDK files and the ${docs.length} install references in the docs hold ${version}:`,
    );
    for (const ref of docs) {
      console.error(`  ${ref.file}:${ref.line}: ${ref.reference}`);
    }
    console.log(`version=${version}\npython=${python}\nprerelease=${version.includes("-")}`);
  } else {
    usage(command === undefined ? "no command" : `unknown command ${command}`);
  }
}

// Node.js before 24.2 has no import.meta.main, and without this check the script would do nothing and exit with 0.
if (import.meta.main === undefined) {
  console.error(
    `This script needs Node.js 24.2 or later, and this is ${process.version}; install a newer Node.js 24.`,
  );
  process.exit(2);
}
if (import.meta.main) {
  main();
}
