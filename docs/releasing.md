# Releasing

A release is a version bump on `main` and an annotated tag on the commit that holds it. The tag starts the two
release workflows, which build and publish everything; nothing is uploaded by hand.

## 1. Prepare the release pull request

On a branch from `main`:

1. **Bump the version.** Set `version` in `[workspace.package]` of the root `Cargo.toml`, then let cargo rewrite the
   workspace crates' entries in `Cargo.lock` without touching any dependency:

   ```sh
   docker run --rm -v "$PWD":/work -v crucible-cargo:/usr/local/cargo/registry \
     crucible-dev:1 cargo update --workspace --offline
   ```

2. **Bump the API version.** Set `info.version` in `specs/openapi/crucible-v1.json` to the same version; an API test
   checks that the two agree. Update the example `"server": {"version": ...}` in `docs/api/README.md`.
3. **Write the release notes.** Move the entries under `## [Unreleased]` in `CHANGELOG.md` into a new
   `## [X.Y.Z] - YYYY-MM-DD` section. cargo-dist uses that section as the body of the GitHub release.

Open the pull request; it merges like any other once every required check passes.

## 2. Tag the merge commit

On the commit of `main` that holds the bump and the notes:

```sh
git fetch origin
git tag -a vX.Y.Z -m "Crucible X.Y.Z" origin/main
git push origin vX.Y.Z
```

A tag such as `v0.2.0` makes a release; a pre-release tag such as `v0.2.0-rc.1` makes a pre-release.

## 3. Check what the tag built

The tag runs two workflows:

| Workflow | Publishes |
|---|---|
| `release.yml` (cargo-dist, configured in `dist-workspace.toml`) | The GitHub release, with the `crucible` binary for seven targets as archives with checksums, a shell installer (`crucible-installer.sh`) and a PowerShell installer (`crucible-installer.ps1`) |
| `docker.yml` | `ghcr.io/elm-os/crucible` for linux/amd64 and linux/arm64, with SBOM and provenance attestations, tagged `X.Y.Z`, `X.Y` and `latest`; a pre-release gets only its own tag |

```sh
gh run list --repo elm-os/crucible --branch vX.Y.Z
gh release view vX.Y.Z --repo elm-os/crucible
docker pull ghcr.io/elm-os/crucible:X.Y.Z
```

The image package on GHCR is public, so the pull works without signing in.

Then confirm that each archive holds `crucible` (`crucible.exe` on Windows), `README.md`, `LICENSE`, `CHANGELOG.md`
and `THIRD_PARTY_NOTICES`. cargo-dist adds the README, the license and the changelog by itself, and `include` in
`dist-workspace.toml` adds `THIRD_PARTY_NOTICES`; every archive of v0.1.0 holds all five.

```sh
curl -fsSL -o crucible.tar.xz \
  https://github.com/elm-os/crucible/releases/download/vX.Y.Z/crucible-x86_64-unknown-linux-gnu.tar.xz
tar tf crucible.tar.xz
```

## 4. Point the docs at the new release

- The quickstart, the README and the self-hosting pages download the Compose files from the release tag
  (`https://raw.githubusercontent.com/elm-os/crucible/vX.Y.Z/deploy/compose/...`) and name the installers on the
  release (`.../releases/download/vX.Y.Z/...`). Replace the old tag in each, then run the quickstart on the
  published image.
- `SECURITY.md` lists the supported release line; update it for a new minor version.
- rightmodeler's Crucible page downloads the Compose file from one constant, `CRUCIBLE_RELEASE_TAG` in
  `apps/web/src/lib/crucible.ts` in the rightmodeler repository; bump it there.
