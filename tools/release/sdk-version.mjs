#!/usr/bin/env node
// The four SDKs carry the server's release version.
//
// Usage:
//   node tools/release/sdk-version.mjs set              write the workspace version (Cargo.toml) into every SDK file
//   node tools/release/sdk-version.mjs check [version]  fail unless the workspace and every SDK file hold the version
//                                                       (the workspace version when none is given); then print it, its
//                                                       Python form and whether it is a pre-release as name=value lines
//
// A version is X.Y.Z or a pre-release X.Y.Z-alpha.N, X.Y.Z-beta.N or X.Y.Z-rc.N. The Python files hold the Python
// form of it: 0.2.0rc1 for 0.2.0-rc.1. Exit code 0 means every file agrees; 1 means one does not; 2 is a usage error.

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..");

// Every place that holds an SDK's version. The pattern's first group is the version, and it must match exactly once.
const manifest = /^ {2}"sdk_version": "([^"]*)",$/m;
const places = [
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

function usage(message) {
  console.error(`${message}\nusage: node tools/release/sdk-version.mjs set | check [version]`);
  process.exit(2);
}

// The Python form of a version, or null when the version is not one this release process uses.
function pythonForm(version) {
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

function read(file) {
  return readFileSync(join(root, file), "utf8");
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
  console.log(failed ? "some files were not updated" : `every SDK file holds ${version}`);
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
  if (problems.length > 0) {
    console.error(problems.join("\n"));
    console.error(
      "Set the workspace version in Cargo.toml, then run `node tools/release/sdk-version.mjs set` (docs/releasing.md).",
    );
    process.exit(1);
  }
  console.error(`the workspace and the ${places.length} SDK files hold ${version}`);
  console.log(`version=${version}\npython=${python}\nprerelease=${version.includes("-")}`);
} else {
  usage(command === undefined ? "no command" : `unknown command ${command}`);
}
