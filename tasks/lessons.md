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
