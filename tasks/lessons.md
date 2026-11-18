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
5. **Fresh pins change the workspace file.** pnpm 11 adds any version younger than its release-age window to
   `minimumReleaseAgeExclude` in `pnpm-workspace.yaml` during install. Pin the newest version older than the window
   instead of keeping an exception; if a lockfile already holds a rejected version, run `pnpm clean --lockfile` and
   reinstall.
6. **Phone widths need emulation.** Headless Chrome never lays out narrower than 500px, so a `--window-size=390,...`
   screenshot is a crop. Use DevTools `Emulation.setDeviceMetricsOverride` for phone-width checks.
7. **Next 16 rewrites `ui/tsconfig.json`** on `next typegen` and `next build` unless `allowJs` and `esModuleInterop` are
   set. Typed routes are slash-less (`/traces`); `trailingSlash: true` adds the slash when links render.
8. **jsdom clicks are not presses.** A UI SDK Tree ran `onAction` for a link label under `fireEvent.click` but not
   under a real press in Chrome, which left the phone drawer open. Check navigation and popup wiring in a real browser
   (DevTools protocol) before reporting it done.
9. **Windows PowerShell 5.1 writes a BOM with `-Encoding utf8`.** A BOM in `package.json` breaks Next and Tailwind
   parsing. Edit JSON and other machine-read files with the Edit tool, Python or POSIX tools, never `Set-Content`.
10. **Check PR text before opening the PR.** The naming checker reads titles and bodies, and its always-flag phrases
    (`policy/naming-rules.json`) fire even when the source is our own product. Run
    `node tools/policy/policy.mjs text` on the body first, and say "inherited from" or "taken from" for our own
    material.
11. **Edit workflow and data files with the Edit tool, never through escaped strings.** A `\n` inside a Python
    heredoc sent through a tool call arrives as a real newline: it split a shell line in `kani.yml`, GitHub could not
    parse the workflow, and the required check vanished from every pull request. The policy workflow now parses every
    workflow file.
12. **A property over one shape must draw values that cannot change the shape.** The renderer property compared two
    value assignments of one plan shape, but the plan keeps filter values as a set, so two equal draws shortened an
    `IN` list and main went red at random. Make a list's values distinct by construction (the second extends the
    first), and check in CI's `cc` line as a `.proptest-regressions` file so the failing case replays first.
