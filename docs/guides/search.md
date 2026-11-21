# Search

**Search** finds spans by the words of their input and output, or by their metadata entries, over the time range
in the header (up to 30 days). The REST API's `POST search` does the same
([API](../api/README.md#searching)), and so does the MCP server's `search_spans` tool ([MCP server](mcp-server.md)).

## Finding spans

Type what to find and choose **Find**. Choose where to look and how the words combine:

| Option | Matches |
|---|---|
| **Input**, **Output** | The words of the span's input or output. Words split at punctuation and spaces and compare without regard to case, so `Lisbon` finds `lisbon` and `"Lisbon"` |
| **Metadata** | Whole `key=value` entries of the span's metadata (`crucible.metadata.<key>` when it was sent), compared as written |
| **All words** (default) | Spans that hold every word |
| **Any word** | Spans that hold at least one |
| **Exact phrase** | Spans that hold the words in order, next to each other |

Results come newest first, each with the span's name, its trace, and the matching passage with the words
highlighted; choosing a result opens the span on its trace page. **Load more results** reads the next page.

The filter bar under the search narrows the results with the conditions of the span list: a name the span's name
contains, its environment, level, type, user, session, a tag, a metadata entry, its release or its version. Each
condition is a chip that removes it, and the conditions stay in the page's address, so a filtered search can be
shared. The API takes the same conditions as the `filters` of `POST search`.

Search reads what has reached ClickHouse, so a span acknowledged a moment ago can be missing from the results for a
few seconds; the page says when spans may still be arriving.

## Reading a payload

A span's input, output, system instructions, tool definitions, tool calls, attributes and events are its payload
fields. The span panel of the trace page shows them, and the API serves each one raw:
`GET projects/{project_id}/spans/{trace_id}/{span_id}/payload?field=<field>&start_time=<start>`
([API](../api/README.md#payloads)). Byte ranges read part of a large field, and `download=1` saves it as a file.

A field that was larger than its cap at ingest is stored cut at a character boundary and listed in the span's
`truncated_fields`; a cut JSON field is no longer valid JSON, so it shows as text
([size caps](sending-traces.md#size-caps)).
