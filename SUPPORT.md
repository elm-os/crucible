# Support

- **Questions** about setting up, configuring or using Crucible: ask in
  [Discussions Q&A](https://github.com/elm-os/crucible/discussions/categories/q-a).
- **Ideas**: suggest one in [Discussions Ideas](https://github.com/elm-os/crucible/discussions/categories/ideas), where
  it can be talked through before it becomes a feature request.
- **Bugs**: open an issue with the
  [bug report template](https://github.com/elm-os/crucible/issues/new?template=bug.yml).
- **Security vulnerabilities**: report them privately, as [SECURITY.md](SECURITY.md) describes, never in a public
  issue.

[Troubleshooting](docs/self-hosting/troubleshooting.md) lists common refusals and their fixes.

## What a bug report needs

- **The version**: the output of `crucible --version`, or of `docker compose exec crucible crucible --version` with
  Docker Compose, and the commit if you built from source.
- **How Crucible is deployed**: Docker Compose or the binary, the ClickHouse and Postgres versions, and the
  operating system.
- **The logs, with secrets removed**: the answer of `/readyz` and the lines of `docker compose logs crucible` around
  the problem, without secret keys, passwords, the master key or any link that holds `?token=`.

[Getting help](docs/getting-help.md) has the details, and the commands that collect each of these.
