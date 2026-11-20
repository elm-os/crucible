## What changed

<!-- The change and why, in plain sentences. Name the contract sections it implements or changes. -->

## How it was verified

<!-- The commands you ran and what they showed, for example the tests, the simulation seeds or the requests to a
running server. -->

## Checklist

- [ ] The title is a Conventional Commit with a lowercase, imperative subject.
- [ ] A change to records, the WAL, the insert protocol, a schema or the API updates
      `docs/architecture/contracts.md` in this pull request.
- [ ] Generated files are regenerated: `specs/openapi/crucible-v1.json` and `ui/src/api/generated/` after an API
      change, `docs/architecture/authz-matrix.md` after a permission change.
- [ ] The docs describe what this change adds or changes.
- [ ] No text says or implies that Crucible copied another product, and nothing carries AI attribution
      (`node tools/policy/policy.mjs files` passes).
