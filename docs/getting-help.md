# Getting help

Questions and ideas go to GitHub Discussions, bugs to GitHub issues, and security vulnerabilities to a private
report. This page says where each one goes and what a bug report needs.

## Where to ask

| You want to | Go to |
|---|---|
| Ask how to set up, configure or use Crucible | [Discussions Q&A](https://github.com/elm-os/crucible/discussions/categories/q-a) |
| Suggest an idea, or talk a change through before anyone builds it | [Discussions Ideas](https://github.com/elm-os/crucible/discussions/categories/ideas) |
| Report something that does not work as these docs say | An issue with the [bug report](https://github.com/elm-os/crucible/issues/new?template=bug.yml) template |
| Propose a specific change, such as a route, a page or a setting | An issue with the [feature request](https://github.com/elm-os/crucible/issues/new?template=feature.yml) template |
| Report a security vulnerability | A private report, as [SECURITY.md](../SECURITY.md) describes, never a public issue |

The web UI links to the same places: **Feedback** in its header opens **Ask a question or suggest an idea**
(Discussions) and **Report a bug** (the issue templates) on GitHub, in a new tab.

Before you ask, [Troubleshooting](self-hosting/troubleshooting.md) lists common refusals and their fixes, and a
search of [Discussions](https://github.com/elm-os/crucible/discussions) and
[issues](https://github.com/elm-os/crucible/issues) may already have the answer.

## What a bug report needs

The bug report template asks for each of these.

1. **The version.** `crucible --version` prints it. With Docker Compose, run
   `docker compose exec crucible crucible --version`. If you built Crucible from source, add the commit
   (`git rev-parse HEAD`).
2. **How Crucible is deployed.** Docker Compose or the binary, the ClickHouse and Postgres versions, and the
   operating system.
3. **What happened.** What you did, what you expected and what happened instead, with the exact error or refusal
   text and the steps that reproduce it. If the web UI showed a **Reference** with the error, include it: a request
   that failed on the server is logged with the same id.
4. **The logs, with secrets removed.** The answer of `/readyz` (`curl -s http://127.0.0.1:8080/readyz`) and the
   log lines around the problem: `docker compose logs crucible` with Docker Compose, or the server's standard error
   with the binary. Before you post anything, remove secret keys (`csk_` and 43 characters), passwords, the master
   key and every link that holds `?token=` (setup, invitation and password-reset links): whoever opens one while it
   is valid can use it.
