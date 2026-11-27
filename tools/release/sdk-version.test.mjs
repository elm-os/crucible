// Tests for sdk-version.mjs. Run them with `node --test tools/release/`.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { docReferences, places, pythonForm } from "./sdk-version.mjs";

const repo = join(dirname(fileURLToPath(import.meta.url)), "..", "..");

// Each reference as its version, with "python " before the Python form of one.
function versions(file, text) {
  return docReferences(file, text).map((ref) => `${ref.python ? "python " : ""}${ref.version}`);
}

test("every kind of install reference is found with its version", () => {
  const cases = [
    [
      "curl -fsSLO https://raw.githubusercontent.com/elm-os/crucible/v0.1.3/deploy/compose/compose.yaml",
      ["0.1.3"],
    ],
    [
      "curl -LsSf https://github.com/elm-os/crucible/releases/download/v0.1.3/crucible-installer.sh | sh",
      ["0.1.3"],
    ],
    [
      "npm install https://github.com/elm-os/crucible/releases/download/v0.1.3/elm-os-crucible-0.1.3.tgz",
      ["0.1.3", "0.1.3"],
    ],
    [
      'pip install "crucible-sdk[mcp] @ https://github.com/elm-os/crucible/releases/download/v0.2.0-rc.1/crucible_sdk-0.2.0rc1-py3-none-any.whl"',
      ["0.2.0-rc.1", "python 0.2.0rc1"],
    ],
    ["the source distribution `crucible_sdk-0.1.3.tar.gz`", ["python 0.1.3"]],
    ["the crate `crucible-sdk-0.2.0-beta.2.crate`", ["0.2.0-beta.2"]],
    ["docker pull ghcr.io/elm-os/crucible:0.1.3", ["0.1.3"]],
    ["CRUCIBLE_IMAGE=ghcr.io/elm-os/crucible:0.2.0-rc.1", ["0.2.0-rc.1"]],
    [
      'crucible-sdk = { git = "https://github.com/elm-os/crucible", tag = "v0.1.3", features = ["mcp"] }',
      ["0.1.3"],
    ],
    ["go get github.com/elm-os/crucible/sdk/go@v0.1.3", ["0.1.3"]],
    ["the Go module tag `sdk/go/v0.1.3`", ["0.1.3"]],
    [
      "Install it from the GitHub release of Crucible 0.1.3; the SDK has the server's version",
      ["0.1.3"],
    ],
    ["Add the module at the version of Crucible 0.1.3-rc.1.", ["0.1.3-rc.1"]],
  ];
  for (const [line, want] of cases) {
    assert.deepEqual(versions("docs/guides/example.md", line), want, line);
  }
});

test("versions of other projects, release lines and minimum versions are not install references", () => {
  const lines = [
    "curl -LsSf https://github.com/axodotdev/cargo-dist/releases/download/v0.34.0/cargo-dist-installer.sh | sh",
    'other = { git = "https://github.com/example/other", tag = "v1.2.3" }',
    "set `CRUCIBLE_IMAGE=ghcr.io/elm-os/crucible:0.1` in `.env`, or `ghcr.io/elm-os/crucible:latest`",
    "It needs Crucible 0.1.3 or newer;",
    "On an upgrade from 0.1.1 these are steps `0001_schema_base` to `0003_access_workspace`",
    "A version such as `v0.1.0` is tagged `0.1.0`, `0.1` and `latest`",
    "Verified with example-exporter 0.17.3 and example-instrumentation 0.1.81 (Python).",
    "npm install /path/to/elm-os-crucible-<version>.tgz",
    "https://github.com/elm-os/crucible/releases/download/vX.Y.Z/elm-os-crucible-X.Y.Z.tgz",
  ];
  for (const line of lines) {
    assert.deepEqual(versions("docs/guides/example.md", line), [], line);
  }
});

test("a reference on the keep list is left out in its own file only", () => {
  const line =
    "go mod edit -require=github.com/elm-os/crucible/sdk/go@v0.0.0 -replace=github.com/elm-os/crucible/sdk/go=../crucible/sdk/go";
  assert.deepEqual(versions("docs/guides/sdk-go.md", line), []);
  assert.deepEqual(versions("docs/guides/example.md", line), ["0.0.0"]);
});

test("a reference carries its line, counted from 1, and the text check prints", () => {
  const [ref] = docReferences(
    "README.md",
    "# Crucible\n\n```sh\ndocker pull ghcr.io/elm-os/crucible:0.1.3\n```\n",
  );
  assert.equal(ref.line, 4);
  assert.equal(ref.reference, "ghcr.io/elm-os/crucible:0.1.3");
});

// A copy of the script, the workspace manifest and every SDK file in a new folder.
function tree(t) {
  const dir = mkdtempSync(join(tmpdir(), "sdk-version-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  for (const file of [
    "tools/release/sdk-version.mjs",
    "Cargo.toml",
    ...places.map((p) => p.file),
  ]) {
    mkdirSync(dirname(join(dir, file)), { recursive: true });
    cpSync(join(repo, file), join(dir, file));
  }
  return dir;
}

function write(dir, file, text) {
  mkdirSync(dirname(join(dir, file)), { recursive: true });
  writeFileSync(join(dir, file), text);
}

function read(dir, file) {
  return readFileSync(join(dir, file), "utf8");
}

function run(dir, ...args) {
  return spawnSync(process.execPath, [join(dir, "tools/release/sdk-version.mjs"), ...args], {
    encoding: "utf8",
  });
}

function setWorkspaceVersion(dir, version) {
  const text = read(dir, "Cargo.toml").replace(
    /^(\[workspace\.package\]\nversion = ")[^"]*"/m,
    (_, head) => `${head}${version}"`,
  );
  write(dir, "Cargo.toml", text);
}

// The docs' install references at a version, with a placeholder on the keep list.
function docs(version, python) {
  return {
    "README.md": `curl -fsSLO https://raw.githubusercontent.com/elm-os/crucible/v${version}/deploy/compose/compose.yaml\n`,
    "sdk/python/README.md": [
      `Install it from the GitHub release of Crucible ${version}:`,
      "",
      `pip install "crucible-sdk[mcp] @ https://github.com/elm-os/crucible/releases/download/v${version}/crucible_sdk-${python}-py3-none-any.whl"`,
      "",
    ].join("\n"),
    "docs/guides/sdk-go.md": [
      `Add the module at the version of Crucible ${version}:`,
      `go get github.com/elm-os/crucible/sdk/go@v${version}`,
      "go mod edit -require=github.com/elm-os/crucible/sdk/go@v0.0.0 -replace=github.com/elm-os/crucible/sdk/go=../crucible/sdk/go",
      "",
    ].join("\n"),
  };
}

// A guide that installs the TypeScript package of a release.
function typescriptGuide(version) {
  return `## Install\n\nnpm install https://github.com/elm-os/crucible/releases/download/v${version}/elm-os-crucible-${version}.tgz\n`;
}

// Files that keep the versions they name: a minimum version, the release steps' examples and the history of releases.
const fixed = {
  "docs/self-hosting/clustered.md": "It needs Crucible 0.1.3 or newer.\n",
  "docs/releasing.md":
    "curl -fsSLO https://github.com/elm-os/crucible/releases/download/v0.1.0/crucible-x86_64-unknown-linux-gnu.tar.xz\n",
  "CHANGELOG.md": "- The SDKs at the Go module tag `sdk/go/v0.1.2` are version 0.1.2.\n",
};

test("set moves the docs' install references to each new version and check accepts them", (t) => {
  const dir = tree(t);
  const version = /^\[workspace\.package\]\nversion = "([^"]*)"$/m.exec(read(dir, "Cargo.toml"))[1];
  const python = pythonForm(version);
  for (const [file, text] of Object.entries({ ...fixed, ...docs(version, python) })) {
    write(dir, file, text);
  }

  const passed = run(dir, "check");
  assert.equal(passed.status, 0, passed.stderr);
  assert.equal(
    passed.stdout,
    `version=${version}\npython=${python}\nprerelease=${version.includes("-")}\n`,
  );
  assert.match(passed.stderr, / and the 6 install references in the docs hold /);
  assert.match(
    passed.stderr,
    /^ {2}README\.md:1: raw\.githubusercontent\.com\/elm-os\/crucible\/v/m,
  );

  // A page that names the previous release, as the docs of every tag so far did.
  write(dir, "docs/guides/sdk-typescript.md", typescriptGuide("0.0.9"));
  const failed = run(dir, "check");
  assert.equal(failed.status, 1);
  assert.equal(failed.stdout, "");
  const expected = version.replaceAll(".", "\\.");
  assert.match(
    failed.stderr,
    new RegExp(
      `^docs/guides/sdk-typescript\\.md:3: github\\.com/elm-os/crucible/releases/download/v0\\.0\\.9/ names 0\\.0\\.9, expected ${expected}$`,
      "m",
    ),
  );
  assert.match(
    failed.stderr,
    new RegExp(
      `^docs/guides/sdk-typescript\\.md:3: elm-os-crucible-0\\.0\\.9\\.tgz names 0\\.0\\.9, expected ${expected}$`,
      "m",
    ),
  );

  for (const next of ["0.2.0-rc.1", "0.2.0"]) {
    setWorkspaceVersion(dir, next);
    const set = run(dir, "set");
    assert.equal(set.status, 0, set.stderr);
    const checked = run(dir, "check");
    assert.equal(checked.status, 0, checked.stderr);
    const nextPython = pythonForm(next);
    assert.equal(
      checked.stdout,
      `version=${next}\npython=${nextPython}\nprerelease=${next.includes("-")}\n`,
    );
    for (const [file, text] of Object.entries({ ...fixed, ...docs(next, nextPython) })) {
      assert.equal(read(dir, file), text, file);
    }
    assert.equal(read(dir, "docs/guides/sdk-typescript.md"), typescriptGuide(next));
  }

  // On files that agree, set changes nothing.
  const again = run(dir, "set");
  assert.equal(again.status, 0, again.stderr);
  assert.doesNotMatch(again.stdout, /->/);
});
