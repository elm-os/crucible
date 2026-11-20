# Security policy

## Reporting a vulnerability

Report a vulnerability privately, through this repository's **Security** tab: choose
[**Report a vulnerability**](https://github.com/elm-os/crucible/security/advisories/new). If you cannot use GitHub,
email [rightmodeler@gmail.com](mailto:rightmodeler@gmail.com). Please do not open a public issue for it.

A report is most useful with:

- the part of Crucible it affects: the server, the MCP proxy, an SDK or the UI;
- the commit or image you ran (`crucible --version` and the commit you built from);
- the steps that reproduce it, and what an attacker gains.

## Supported versions

Fixes are made on the `main` branch. A deployment takes a fix by building its image or binary from a commit that has
it and upgrading ([operations](docs/self-hosting/operations.md#upgrades)).

## Security model

What Crucible protects by default, and the settings that harden a deployment, are described in
[docs/self-hosting/security.md](docs/self-hosting/security.md).
