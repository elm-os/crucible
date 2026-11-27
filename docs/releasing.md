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

   Then give the four SDKs and the docs the same version. `set` writes it into each SDK's manifest, version constant,
   lock file and conformance manifest, and into every install reference in the docs, so the docs at the tag install
   the release that the tag makes. `check` fails while any of them names another version, and otherwise lists the
   install references it found:

   ```sh
   node tools/release/sdk-version.mjs set
   node tools/release/sdk-version.mjs check
   ```

   An install reference is a place in `README.md`, `sdk/*/README.md` or `docs/**/*.md` that names a release: a
   download from the GitHub release (`https://github.com/elm-os/crucible/releases/download/vX.Y.Z/...`), an SDK
   package's file name, a Compose file at the release tag
   (`https://raw.githubusercontent.com/elm-os/crucible/vX.Y.Z/deploy/compose/...`), the image
   (`ghcr.io/elm-os/crucible:X.Y.Z`), the Rust git tag (`tag = "vX.Y.Z"`), the Go module (`sdk/go@vX.Y.Z` and
   `sdk/go/vX.Y.Z`), or the words "release of Crucible X.Y.Z" or "version of Crucible X.Y.Z", with which the SDK
   guides and READMEs name the release they install. A reference that names another version on purpose, such as a
   placeholder or an upgrade note about an older release, goes on the `keep` list in `tools/release/sdk-version.mjs`.
   `CHANGELOG.md` and this page are never changed.

2. **Bump the API version.** Set `info.version` in `specs/openapi/crucible-v1.json` to the same version; an API test
   checks that the two agree. Update the example `"server": {"version": ...}` in `docs/api/README.md`.
3. **Write the release notes.** Move the entries under `## [Unreleased]` in `CHANGELOG.md` into a new
   `## [X.Y.Z] - YYYY-MM-DD` section. cargo-dist uses that section as the body of the GitHub release.
4. **Update the supported versions** in `SECURITY.md` for a new minor version; it lists the supported release line.

Open the pull request; it merges like any other once every required check passes. The `sdk-release` workflow runs on
it and fails while an SDK file or an install reference in the docs names another version than `Cargo.toml`.

From the merge until the release is published, the docs on `main` name a release that is not out. Its downloads
(`https://github.com/elm-os/crucible/releases/download/vX.Y.Z/...`) return 404 until `release.yml` publishes the
release, and the Compose files at the tag, the image and the Go module appear with the tag and its workflows. A link
check in that window reports them as broken: skip the URLs of a version newer than the latest published release,
which `gh release view --repo elm-os/crucible --json tagName --jq .tagName` prints, until the release is published.
Tag the merge commit as soon as `main` is green, so the window stays short.

## 2. Tag the merge commit

Tag the commit of `main` that holds the bump and the notes as soon as its checks pass. Run `check` on it first: a pull
request merged after the release pull request can name the previous version again, and the tag's `sdk-release` run
fails on that before it builds any package.

```sh
git fetch origin
git switch --detach origin/main
node tools/release/sdk-version.mjs check X.Y.Z
git tag -a vX.Y.Z -m "Crucible X.Y.Z" origin/main
git push origin vX.Y.Z
```

A tag such as `v0.2.0` makes a release; a pre-release tag such as `v0.2.0-rc.1` makes a pre-release.

## 3. Check what the tag built

The tag runs three workflows:

| Workflow | Publishes |
|---|---|
| `release.yml` (cargo-dist, configured in `dist-workspace.toml`) | The GitHub release, with the `crucible` binary for seven targets as archives with checksums, a shell installer (`crucible-installer.sh`), a PowerShell installer (`crucible-installer.ps1`) and `dist-manifest.json`, cargo-dist's description of the release |
| `docker.yml` | `ghcr.io/elm-os/crucible` for linux/amd64 and linux/arm64, with SBOM and provenance attestations, tagged `X.Y.Z`, `X.Y` and `latest`; a pre-release gets only its own tag |
| `sdk-release.yml` | The SDK packages on the GitHub release: `elm-os-crucible-X.Y.Z.tgz` (TypeScript), `crucible_sdk-X.Y.Z-py3-none-any.whl` and `crucible_sdk-X.Y.Z.tar.gz` (Python) and `crucible-sdk-X.Y.Z.crate` (Rust); the Go module tag `sdk/go/vX.Y.Z` on the release commit; and each registry that [Registry setup](#registry-setup) turns on |

`sdk-release.yml` builds and checks the packages, waits until `release.yml` has created the release, and then
attaches them, so it finishes shortly after `release.yml`. It adds them to a release that is already published, so
the repository's immutable releases setting has to stay off.

```sh
gh run list --repo elm-os/crucible --branch vX.Y.Z
gh release view vX.Y.Z --repo elm-os/crucible
docker pull ghcr.io/elm-os/crucible:X.Y.Z
go list -m github.com/elm-os/crucible/sdk/go@vX.Y.Z
```

The image package on GHCR is public, so the pull works without signing in.

Check that the release lists `dist-manifest.json`. This prints its name, or nothing when it is missing:

```sh
gh release view vX.Y.Z --repo elm-os/crucible --json assets --jq '.assets[].name | select(. == "dist-manifest.json")'
```

If it is missing, attach the copy that the release run stored as the artifact `artifacts-dist-manifest`. The first
command lists the run of the tag; use its ID in the second:

```sh
gh run list --repo elm-os/crucible --workflow release.yml --branch vX.Y.Z
gh run download <run ID> --repo elm-os/crucible -n artifacts-dist-manifest
gh release upload vX.Y.Z dist-manifest.json --repo elm-os/crucible
```

If an `sdk-release.yml` job fails, fix the cause and run the workflow again from the tag: on the Actions tab, open
sdk-release, choose Run workflow and pick the tag under "Use workflow from", or run
`gh workflow run sdk-release.yml --repo elm-os/crucible --ref vX.Y.Z`. The run replaces the SDK packages on the
release and leaves alone a version that a registry already has. Never push an `sdk/go/` tag by hand:
`release.yml` starts on any tag that ends in a version, so it would publish a second release with the binaries. The
workflow creates the tag with its own token, which starts no workflow, once every build and check of the run has
passed.

Then confirm that each archive holds `crucible` (`crucible.exe` on Windows), `README.md`, `LICENSE`, `CHANGELOG.md`
and `THIRD_PARTY_NOTICES`. cargo-dist adds the README, the license and the changelog by itself, and `include` in
`dist-workspace.toml` adds `THIRD_PARTY_NOTICES`; every archive of v0.1.0 holds all five.

```sh
curl -fsSL -o crucible.tar.xz \
  https://github.com/elm-os/crucible/releases/download/vX.Y.Z/crucible-x86_64-unknown-linux-gnu.tar.xz
tar tf crucible.tar.xz
```

A release tagged before `sdk-release.yml` existed has no SDK packages, and its tag cannot start the workflow. While
the SDK files on `main` hold that release's version, which they do until the next release pull request, run the
workflow from `main` with the tag as input:

```sh
gh workflow run sdk-release.yml --repo elm-os/crucible --ref main -f tag=vX.Y.Z
```

That run builds and checks the packages from the head of `main`, attaches them to the release, tags the Go module on
that commit and publishes to each registry that is turned on. It refuses the input on another branch, for a tag
without a release, and for a tag that holds the workflow, which runs from its own tag.

## 4. After the release is published

- Run the [quickstart](self-hosting/quickstart.md) on the published image; its commands download the Compose file
  from the new tag.
- rightmodeler's Crucible page downloads the Compose file from one constant, `CRUCIBLE_RELEASE_TAG` in
  `apps/web/src/lib/crucible.ts` in the rightmodeler repository. Bump it once the release is published, since the
  page links to the release's published files.

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
secret or a protection rule, such as a required reviewer. A rule that limits where deployments come from has to
allow `v*` tags and the `main` branch, since a release tagged before the workflow existed publishes from `main`.

To publish the current release after a step below, run the workflow for it. When its tag holds `sdk-release.yml`,
run it from the tag:

```sh
gh workflow run sdk-release.yml --repo elm-os/crucible --ref vX.Y.Z
```

Otherwise run it from `main` with the tag as input, as [step 3](#3-check-what-the-tag-built) describes:

```sh
gh workflow run sdk-release.yml --repo elm-os/crucible --ref main -f tag=vX.Y.Z
```

### PyPI

PyPI creates the project on its first trusted publish, so no token is needed at any point.

1. Create an account at https://pypi.org/account/register/ and turn on two-factor authentication.
2. Open the Publishing page of your account (https://pypi.org/manage/account/publishing/) and add a pending publisher
   for GitHub: PyPI project name `crucible-sdk`, owner `elm-os`, repository name `crucible`, workflow name
   `sdk-release.yml`, environment name `pypi`. A pending publisher does not reserve the name; its first use creates
   the project.
3. Set the repository variable `PUBLISH_PYPI` to `true`.
4. Run the workflow for the current release, as above. The `pypi` job uploads the wheel and the source distribution
   with their attestations to https://pypi.org/p/crucible-sdk.

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
   `sdk-release.yml` and environment name `npm`, and allow `npm publish` under Allowed actions. With npm 11.15 or
   later, this command does the same:

   ```sh
   npm trust github @elm-os/crucible --file sdk-release.yml --repo elm-os/crucible --env npm --allow-publish
   ```

   Then set the repository variable `PUBLISH_NPM` to `true`. The tag publishes the new version with a provenance
   statement, and that publish confirms the publisher. If the publisher lapses before a release uses it, delete it
   and add it again.
5. Once a trusted publish has succeeded, you can choose "Require two-factor authentication and disallow tokens" under
   the package's Settings, Publishing access; trusted publishing keeps working.

### crates.io

crates.io takes a crate's first version only with an API token, so the first publish runs the workflow with a token
that you delete right after.

1. Sign in at https://crates.io with GitHub, and verify your email address at https://crates.io/settings/profile.
2. Create an API token at https://crates.io/settings/tokens/new: expiration 7 days, scopes `publish-new` and
   `publish-update`, and the crate pattern `crucible-sdk`.
3. In the repository's settings on GitHub, under Environments, create the environment `crates-io` and add the
   environment secret `CARGO_REGISTRY_TOKEN` holding the token. Set the repository variable `PUBLISH_CRATES` to
   `true`.
4. Run the workflow for the current release, as above. With the secret present, the `crates` job publishes with it.
5. When the crate is on https://crates.io/crates/crucible-sdk: open its Settings, Trusted Publishing, Add, GitHub, and
   enter owner `elm-os`, repository `crucible`, workflow filename `sdk-release.yml` and environment `crates-io`.
6. Delete the `CARGO_REGISTRY_TOKEN` secret and revoke the token on crates.io. Later releases publish with trusted
   publishing. To refuse publishes made with a token from then on, tick "Require trusted publishing for all new
   versions" in the crate's Settings, under Trusted Publishing.

### Go

The Go module needs no account: every release tags it as `sdk/go/vX.Y.Z`, and `go get` fetches it through
proxy.golang.org, which pkg.go.dev reads.

### Once a package is on its registry

Change the install command in that SDK's guide and README to the registry's (`npm install @elm-os/crucible`,
`pip install "crucible-sdk[mcp]"`, `cargo add crucible-sdk --features mcp`) and keep the release download as the
other way.
