#!/usr/bin/env python3
"""Proves that the determinism bans are live.

bans   Runs clippy on tools/ban-selftest with the root clippy.toml and fails unless every disallowed
       method and type listed there is planted in src/lib.rs and reported on its planted line.
guard  Extracts the allow-attribute guard from .github/workflows/ci-rust.yml and fails unless it flags a
       fixture that switches off a determinism lint outside the allowlist (and passes the controls).

Run from anywhere: `python3 tools/ban-selftest/check.py [bans|guard]` (both when no argument is given).
"""

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
PLANTED = HERE / "src" / "lib.rs"
# Entries of clippy.toml that cannot resolve in this crate's dependency graph, one path per line.
UNRESOLVED = HERE / "unresolved.txt"
LINTS = {"method": "clippy::disallowed_methods", "type": "clippy::disallowed_types"}


def fail(message):
    sys.stdout.flush()
    print(f"ban-selftest: FAIL: {message}", file=sys.stderr)
    sys.exit(1)


def configured():
    config = tomllib.loads((ROOT / "clippy.toml").read_text(encoding="utf-8"))
    entries = set()
    for kind, key in (("method", "disallowed-methods"), ("type", "disallowed-types")):
        for entry in config.get(key, []):
            entries.add((kind, entry if isinstance(entry, str) else entry["path"]))
    return entries


def planted():
    marks = {}
    for number, line in enumerate(PLANTED.read_text(encoding="utf-8").splitlines(), start=1):
        found = re.search(r"// ban-(method|type): (\S+)\s*$", line)
        if found:
            key = (found.group(1), found.group(2))
            if key in marks:
                fail(f"{found.group(2)} is planted twice")
            marks[key] = number
    return marks


def clippy_messages():
    env = dict(os.environ, CLIPPY_CONF_DIR=str(ROOT))
    command = ["cargo", "clippy", "--message-format=json", "--manifest-path", str(HERE / "Cargo.toml")]
    run = subprocess.run(command, env=env, stdout=subprocess.PIPE, text=True, encoding="utf-8")
    if run.returncode != 0:
        fail(f"clippy exited with {run.returncode}; the planted crate must compile")
    messages = []
    for line in run.stdout.splitlines():
        if line.startswith("{"):
            record = json.loads(line)
            if record.get("reason") == "compiler-message":
                messages.append(record["message"])
    return messages


def check_bans():
    entries = configured()
    marks = planted()
    unresolved = set()
    if UNRESOLVED.exists():
        for line in UNRESOLVED.read_text(encoding="utf-8").splitlines():
            if line.strip() and not line.startswith("#"):
                kind, path = line.split()
                unresolved.add((kind, path))

    missing = entries - set(marks) - unresolved
    stale = (set(marks) | unresolved) - entries
    if missing:
        fail(f"clippy.toml entries with no planted use: {sorted(missing)}")
    if stale:
        fail(f"planted or listed paths that clippy.toml no longer bans: {sorted(stale)}")
    if set(marks) & unresolved:
        fail(f"paths both planted and listed as unresolved: {sorted(set(marks) & unresolved)}")

    reported = {}  # (lint, line) -> message
    for message in clippy_messages():
        text = message.get("message", "")
        if "does not refer to" in text:
            # clippy could not resolve a configured path: the ban is dead in this crate.
            path = re.search(r"`([^`]+)`", text)
            if not path or not any(p == path.group(1) for _, p in unresolved):
                fail(f"clippy cannot resolve a configured path: {text}")
        code = (message.get("code") or {}).get("code")
        if code in LINTS.values():
            for span in message.get("spans", []):
                if span.get("is_primary") and span["file_name"].replace("\\", "/").endswith("src/lib.rs"):
                    reported[(code, span["line_start"])] = text

    silent = []
    for (kind, path), line in sorted(marks.items(), key=lambda item: item[1]):
        text = reported.pop((LINTS[kind], line), None)
        if text is None:
            silent.append(f"{path} (src/lib.rs:{line})")
        else:
            print(f"  reported  {kind:6}  {path:45}  {text}")
    if silent:
        fail(f"planted uses that clippy did not report: {silent}")
    if reported:
        fail(f"disallowed-use reports on unmarked lines: {sorted(reported.items())}")

    methods = sum(1 for kind, _ in marks if kind == "method")
    print(
        f"ban-selftest: bans ok: {len(entries)} clippy.toml entries = {len(marks)} planted "
        f"({methods} methods, {len(marks) - methods} types) + {len(unresolved)} unresolved; "
        f"{len(marks)} of {len(marks)} planted uses reported"
    )


def guard_script():
    """The `run:` block of the allow-attribute guard step, verbatim."""
    lines = (ROOT / ".github" / "workflows" / "ci-rust.yml").read_text(encoding="utf-8").splitlines()
    for index, line in enumerate(lines):
        if re.match(r"\s*- name: Allow-attribute guard\s*$", line):
            break
    else:
        fail("ci-rust.yml has no step named 'Allow-attribute guard'")
    run = lines[index + 1]
    if not re.match(r"\s*run: \|\s*$", run):
        fail("the guard step is not a `run: |` block")
    indent = len(run) - len(run.lstrip()) + 2
    body = []
    for line in lines[index + 2 :]:
        if line.strip() and len(line) - len(line.lstrip()) < indent:
            break
        body.append(line[indent:])
    script = "\n".join(body).strip() + "\n"
    if "disallowed_" not in script or "grep" not in script:
        fail("the extracted guard does not look like the guard")
    return script


def run_guard(script, files):
    """Runs the guard in a scratch tree holding `files` ({relative path: content}); returns (code, output)."""
    with tempfile.TemporaryDirectory() as scratch:
        for relative, content in files.items():
            target = Path(scratch) / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content, encoding="utf-8", newline="\n")
        for directory in ("crates", "sdk"):
            (Path(scratch) / directory).mkdir(exist_ok=True)
        run = subprocess.run(
            [shutil.which("bash") or "bash", "-eo", "pipefail", "-c", script],
            cwd=scratch,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        return run.returncode, run.stdout


def check_guard():
    script = guard_script()
    fixture = (HERE / "fixtures" / "expect_outside_allowlist.rs").read_text(encoding="utf-8")
    clean = "pub fn nothing() {}\n"
    cases = [
        # (description, files, must the guard fail?, path that must be named)
        ("expect attribute in a product crate", {"crates/crucible-wal/src/lib.rs": fixture}, True, "crates/crucible-wal/src/lib.rs"),
        ("expect attribute in an SDK", {"sdk/rust/src/lib.rs": fixture}, True, "sdk/rust/src/lib.rs"),
        ("allow attribute in a product crate", {"crates/crucible-wal/src/x.rs": fixture.replace("#[expect(", "#[allow(")}, True, "crates/crucible-wal/src/x.rs"),
        ("expect attribute inside the allowlist", {"crates/crucible-env-prod/src/lib.rs": fixture, "crates/crucible/src/serve.rs": fixture, "crates/crucible-auth/src/scope.rs": fixture, "crates/crucible-wal/benches/b.rs": fixture, "crates/crucible-loadgen/src/main.rs": fixture}, False, None),
        ("no attribute anywhere", {"crates/crucible-wal/src/lib.rs": clean}, False, None),
    ]
    for description, files, must_fail, named in cases:
        code, output = run_guard(script, files)
        if must_fail and (code == 0 or named not in output):
            fail(f"guard did not flag: {description} (exit {code})\n{output}")
        if not must_fail and code != 0:
            fail(f"guard flagged a permitted tree: {description}\n{output}")
        print(f"  guard {'flags ' if must_fail else 'passes'}  {description}")
    print(f"ban-selftest: guard ok: {len(cases)} cases")


def main():
    which = sys.argv[1:] or ["bans", "guard"]
    for name in which:
        if name == "bans":
            check_bans()
        elif name == "guard":
            check_guard()
        else:
            fail(f"unknown check {name!r}")


if __name__ == "__main__":
    main()
