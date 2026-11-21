# Releasing

A release is a version bump on `main` and an annotated tag on the commit that holds it. The tag starts the three
release workflows, which build and publish everything; nothing is uploaded by hand. The four SDKs have the server's
version and ship with it.

## 1. Prepare the release pull request

On a branch from `main`:

1. **Bump the version.** Set `version` in `[workspace.package]` of the root `Cargo.toml`, then let cargo rewrite the
   workspace crates' entries in `Cargo.lock` without touching any dependency:

   ```sh
   docker run --rm -v "$PWD":/work -v crucible-cargo:/usr/local/cargo/registry \
     crucible-dev:1 cargo update --workspace --offline
   ```

   Then give the four SDKs the same version. The script writes it into each SDK's manifest, version constant, lock
   file and conformance manifest, and `check` lists every file it covers:

   ```sh
   node tools/release/sdk-version.mjs set
   node tools/release/sdk-version.mjs check
   ```

2. **Bump the API version.** Set `info.version` in `specs/openapi/crucible-v1.json` to the same version; an API test
   checks that the two agree. Update the example `"server": {"version": ...}` in `docs/api/README.md`.
3. **Write the release notes.** Move the entries under `## [Unreleased]` in `CHANGELOG.md` into a new
   `## [X.Y.Z] - YYYY-MM-DD` section. cargo-dist uses that section as the body of the GitHub release.

Open the pull request; it merges like any other once every required check passes. The `sdk-release` workflow runs on
it and fails while an SDK file holds another version than `Cargo.toml`.

## 2. Tag the merge commit

On the commit of `main` that holds the bump and the notes:

```sh
git fetch origin
git tag -a vX.Y.Z -m "Crucible X.Y.Z" origin/main
git push origin vX.Y.Z
```

A tag such as `v0.2.0` makes a release; a pre-release tag such as `v0.2.0-rc.1` makes a pre-release.

## 3. Check what the tag built

The tag runs three workflows:

| Workflow | Publishes |
|---|---|
| `release.yml` (cargo-dist, configured in `dist-workspace.toml`) | The GitHub release, with the `crucible` binary for seven targets as archives with checksums, a shell installer (`crucible-installer.sh`) and a PowerShell installer (`crucible-installer.ps1`) |
| `docker.yml` | `ghcr.io/elm-os/crucible` for linux/amd64 and linux/arm64, with SBOM and provenance attestations, tagged `X.Y.Z`, `X.Y` and `latest`; a pre-release gets only its own tag |
| `sdk-release.yml` | The SDK packages on the GitHub release: `elm-os-crucible-X.Y.Z.tgz` (TypeScript), `crucible_sdk-X.Y.Z-py3-none-any.whl` and `crucible_sdk-X.Y.Z.tar.gz` (Python) and `crucible-sdk-X.Y.Z.crate` (Rust); the Go module tag `sdk/go/vX.Y.Z` on the release commit; and each registry that [Registry setup](#registry-setup) turns on |

`sdk-release.yml` builds and checks the packages, waits until `release.yml` has created the release, and then
attaches them, so it finishes shortly after `release.yml`.

```sh
gh run list --repo elm-os/crucible --branch vX.Y.Z
gh release view vX.Y.Z --repo elm-os/crucible
docker pull ghcr.io/elm-os/crucible:X.Y.Z
go list -m github.com/elm-os/crucible/sdk/go@vX.Y.Z
```

The image package on GHCR is public, so the pull works without signing in.

If an `sdk-release.yml` job fails, fix the cause and run the workflow again from the tag: on the Actions tab, open
sdk-release, choose Run workflow and pick the tag under "Use workflow from", or run
`gh workflow run sdk-release.yml --repo elm-os/crucible --ref vX.Y.Z`. The run replaces the SDK packages on the
release and leaves alone a version that a registry already has. Never push an `sdk/go/` tag by hand:
`release.yml` starts on any tag that ends in a version, so it would publish a second release with the binaries. The
workflow creates the tag with its own token, which starts no workflow.

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
- The SDK guides (`docs/guides/sdk-typescript.md`, `docs/guides/sdk-python.md` and `docs/guides/sdk-go.md`) and the
  READMEs in `sdk/typescript`, `sdk/python`, `sdk/go` and `sdk/rust` install the SDKs from the release: the package
  downloads (`.../releases/download/vX.Y.Z/...`), the Go module tag (`@vX.Y.Z`) and the Rust git tag
  (`tag = "vX.Y.Z"`). Replace the old version in each.
- `SECURITY.md` lists the supported release line; update it for a new minor version.
- rightmodeler's Crucible page downloads the Compose file from one constant, `CRUCIBLE_RELEASE_TAG` in
  `apps/web/src/lib/crucible.ts` in the rightmodeler repository; bump it there.

## Registry setup

`sdk-release.yml` publishes the TypeScript package to npm as `@elm-os/crucible`, the Python package to PyPI as
`crucible-sdk` and the Rust crate to crates.io as `crucible-sdk`, each once the project owner has set up that
registry. Until then the registry's job is skipped and the rest of the workflow runs. Each registry trusts the
workflow itself (trusted publishing over OpenID Connect), so the repository stores no long-lived registry token. Every
registry asks for the same values:

| Field | Value |
|---|---|
| Owner (GitHub organization) | `elm-os` |
| Repository | `crucible` |
| Workflow file | `sdk-release.yml` |
| Environment | `npm`, `pypi` or `crates-io`, as below |

A repository variable turns a registry's job on: set it under Settings, Secrets and variables, Actions, Variables, or
with `gh variable set PUBLISH_PYPI --repo elm-os/crucible --body true`. GitHub creates the `npm`, `pypi` and
`crates-io` environments the first time a job uses them; create one yourself under Settings, Environments to add a
protection rule, such as limiting it to `v*` tags, or a secret.

To publish the current release after a step below, run the workflow from its tag:

```sh
gh workflow run sdk-release.yml --repo elm-os/crucible --ref vX.Y.Z
```

### PyPI

PyPI creates the project on its first trusted publish, so no token is needed at any point.

1. Create an account at https://pypi.org/account/register/ and turn on two-factor authentication.
2. Open the Publishing page of your account (https://pypi.org/manage/account/publishing/) and add a pending publisher
   for GitHub: PyPI project name `crucible-sdk`, owner `elm-os`, repository name `crucible`, workflow name
   `sdk-release.yml`, environment name `pypi`. A pending publisher does not reserve the name; its first use creates
   the project.
3. Set the repository variable `PUBLISH_PYPI` to `true`.
4. Run the workflow from the current release's tag. The `pypi` job uploads the wheel and the source distribution with
   their attestations to https://pypi.org/p/crucible-sdk.

### npm

npm adds a trusted publisher only to a package that exists, and drops one that no publish uses within 2 days. So the
first version goes up by hand from the release, and the trusted publisher follows on the day of the next release.

1. Create an account at https://www.npmjs.com/signup and turn on two-factor authentication.
2. Create the organization `elm-os`: your profile picture, Add an Organization, name `elm-os`, and the free plan,
   Unlimited public packages.
3. Publish the current release's package from your computer (npm asks for your second factor):

   ```sh
   npm login
   npm publish https://github.com/elm-os/crucible/releases/download/vX.Y.Z/elm-os-crucible-X.Y.Z.tgz --access public
   ```

4. On the day of the next release, before you push its tag: on npmjs.com open `@elm-os/crucible`, Settings, Trusted
   publishing, and add a GitHub Actions publisher with organization `elm-os`, repository `crucible`, workflow filename
   `sdk-release.yml` and environment name `npm`, and allow `npm publish` under Allowed actions. Then set the
   repository variable `PUBLISH_NPM` to `true`. The tag publishes the new version with a provenance statement, and
   that publish confirms the publisher. If the publisher lapses before a release uses it, delete it and add it again.
5. Once a trusted publish has succeeded, you can choose "Require two-factor authentication and disallow tokens" under
   the package's Settings, Publishing access; trusted publishing keeps working.

### crates.io

crates.io takes a crate's first version only with an API token, so the first publish runs the workflow with a token
that you delete right after.

1. Sign in at https://crates.io with GitHub, and verify your email address at https://crates.io/settings/profile.
2. Create an API token at https://crates.io/settings/tokens/new: expiration 7 days, scopes `publish-new` and
   `publish-update`, and the crate pattern `crucible-sdk`.
3. Under Settings, Environments, create the environment `crates-io` and add the environment secret
   `CARGO_REGISTRY_TOKEN` holding the token. Set the repository variable `PUBLISH_CRATES` to `true`.
4. Run the workflow from the current release's tag. With the secret present, the `crates` job publishes with it.
5. When the crate is on https://crates.io/crates/crucible-sdk: open its Settings, Trusted Publishing, Add, GitHub, and
   enter owner `elm-os`, repository `crucible`, workflow filename `sdk-release.yml` and environment `crates-io`.
6. Delete the `CARGO_REGISTRY_TOKEN` secret and revoke the token on crates.io. Later releases publish with trusted
   publishing; the crate's settings can then refuse publishes made with a token.

### Go

The Go module needs no account: every release tags it as `sdk/go/vX.Y.Z`, and `go get` fetches it through
proxy.golang.org, which pkg.go.dev reads.

### Once a package is on its registry

Change the install command in that SDK's guide and README to the registry's (`npm install @elm-os/crucible`,
`pip install "crucible-sdk[mcp]"`, `cargo add crucible-sdk --features mcp`) and keep the release download as the
other way.
