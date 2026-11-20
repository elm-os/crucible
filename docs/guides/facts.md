# Tags, flags, scores and corrections

Facts are what people and programs add to telemetry after it arrived: tags, the bookmark and public flags, scores
(feedback) and corrected outputs. They attach to a trace, a span or a session, are stored apart from the spans,
which never change, and show wherever the target shows: the trace list, the trace page and the API.

## In the UI

The trace page has a **Tags, flags and feedback** section. There you add or remove a tag (up to 256 bytes),
switch **Bookmark** and **Public**, and add a score with a name, a value (a number, `true` or `false`, or a word) and
a comment. Writing facts needs the member role on the project; viewers see them.

## Through the API

Facts are written with a project key that has the `write` scope, or a signed-in session, and read with the `read`
scope. A write names its target with `trace_id` (a trace), `trace_id` and `span_id` (a span), or `session_id` (a
session):

```sh
export CRUCIBLE_URL=http://127.0.0.1:8080
export PROJECT_ID=<project id>
export CRUCIBLE_SECRET_KEY=csk_...

# A score on a trace. Retrying with the same Idempotency-Key returns the first answer.
curl -s -X POST "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/feedback" \
  -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: thumbs-4bf9' \
  -d '{"trace_id": "4bf92f3577b34da6a3ce929d0e0e4736", "name": "helpful", "value": 1, "comment": "good answer"}'
```

```json
{"fact_id":"01a1f65b-b4a4-70eb-83eb-6dc856127f2d","key":"helpful","version":1795134567588787,"created_at":"2026-11-20T00:29:27.588787Z","target_time":"2026-11-19T23:45:01.000000Z"}
```

| Route | Body | Writes |
|---|---|---|
| `POST projects/{project_id}/feedback` | `name`, `value`, and optionally `score_id`, `data_type`, `comment` (4 KiB), `metadata` (a JSON object, 16 KiB), `retracted` | A score with source `feedback`. The value is a number (`numeric`), a boolean (`boolean`) or a string (`categorical`, or `text` with `data_type: "text"`) |
| `POST projects/{project_id}/tags` | `tag` (1 to 256 bytes), `op`: `add` or `remove` | A tag edit. Removing a tag the trace was sent with hides it |
| `POST projects/{project_id}/flags` | `flag`: `bookmarked` or `public`, `value` | A flag |
| `POST projects/{project_id}/corrections` | `corrected` (any JSON value), and optionally `note`, `retracted` | The corrected output of a span or trace |
| `GET projects/{project_id}/facts` | query `trace_id`, `span_id` or `session_id` | The effective facts of one target, each with who wrote it and when |

Every write answers 201 with the fact's id, its key, a `version` and the target's time, once the fact is durable in
the write-ahead log; reads show it once it reaches ClickHouse, within a few seconds. All four take an
`Idempotency-Key` header (1 to 128 characters of `[A-Za-z0-9_-]`), honoured for 24 hours.

## How facts resolve

Facts are append-only. The latest write of a key on a target wins:

- a score is keyed by its name (and `score_id`, so several scores of one name stay apart);
- `retracted: true` removes the key's value: the target has no score of that name, or no correction, afterwards;
- a trace's tags are the tags it was sent with (`crucible.trace.tags`), plus the tags whose latest edit is `add`,
  minus those whose latest edit is `remove`.

After adding `reviewed` to a trace that was sent with the tag `beta`, removing `beta`, bookmarking it and scoring
it, its facts read:

```sh
curl -s -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/facts?trace_id=4bf92f3577b34da6a3ce929d0e0e4736"
```

```json
{
  "scores": [
    {
      "name": "helpful",
      "score_id": null,
      "data_type": "numeric",
      "value": 1.0,
      "source": "feedback",
      "comment": "good answer",
      "metadata": null,
      "fact": {
        "fact_id": "01a1f65b-b4a4-70eb-83eb-6dc856127f2d",
        "version": 1795134567588787,
        "created_at": "2026-11-20T00:29:27.588787Z",
        "author_kind": "api_key",
        "author_id": "01a1f633-d84d-7d84-9ab9-d78f6df778d4"
      }
    }
  ],
  "tags": [
    {"tag": "beta", "op": "remove", "fact": {"fact_id": "01a1f65b-b556-7932-83d5-cf2148c557bd", "version": 1795134567766143, "created_at": "2026-11-20T00:29:27.766143Z", "author_kind": "api_key", "author_id": "01a1f633-d84d-7d84-9ab9-d78f6df778d4"}},
    {"tag": "reviewed", "op": "add", "fact": {"fact_id": "01a1f65b-b524-7c54-955c-537c38099144", "version": 1795134567716041, "created_at": "2026-11-20T00:29:27.716041Z", "author_kind": "api_key", "author_id": "01a1f633-d84d-7d84-9ab9-d78f6df778d4"}}
  ],
  "bookmarked": {"value": true, "fact": {"fact_id": "01a1f65b-b579-7092-81bd-293d4c34e26a", "version": 1795134567801282, "created_at": "2026-11-20T00:29:27.801282Z", "author_kind": "api_key", "author_id": "01a1f633-d84d-7d84-9ab9-d78f6df778d4"}},
  "public": null,
  "correction": null,
  "meta": {"as_of": "2026-11-20T00:29:28.978765Z", "partial": false, "consistency": "strong"}
}
```

The trace list and the trace page show the resolved values: the row of that trace has `tags: ["reviewed"]` and a
`facts` object with the tags, `bookmarked: true`, the scores (without comment and metadata) and the correction.
`author_kind` is `user` for a signed-in person and `api_key` for a key, with the user's or the key's id.

The SQL workspace reads every edit, retracted ones included, in `crucible_sql.scores`, `crucible_sql.tags`,
`crucible_sql.flags` and `crucible_sql.corrections`.
