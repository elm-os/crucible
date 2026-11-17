# Lessons

Rules learned from corrections and surprises. Read at the start of each session.

1. **Windows path limit.** Large JavaScript repositories exceed 260-character paths. Vendor them under `C:\src\` with
   `core.longpaths=true`, never under deep user-profile folders.
2. **No MSVC on the host.** Rust builds, tests, simulation, Kani and fuzzing run in the `crucible-dev` container.
   Installs that need an elevation prompt are deferred until a person is present.
3. **Policy tooling must run everywhere.** The naming and attribution checker is a zero-dependency Node script so git
   hooks work on any host without a Rust toolchain.
4. **Describe rules without quoting them.** Docs that explain the naming rule refer to `policy/naming-rules.json`
   instead of repeating the flagged phrases.
