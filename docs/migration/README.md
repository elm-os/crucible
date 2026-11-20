# Migration

Moving an application's tracing to Crucible changes where its spans are exported and which key they carry. The
instrumentation stays, and so do the spans' attributes: Crucible maps them as they arrive
([integrations](../integrations/README.md)).

| From | Page |
|---|---|
| The Langfuse SDKs (Python and JavaScript) | [Moving from Langfuse](langfuse.md) |
| Braintrust's OpenTelemetry attributes, from an OTLP exporter or a Braintrust SDK | [Moving from Braintrust](braintrust.md) |

Only spans exported after the switch reach Crucible. Features that the source product serves from its own REST API
rather than from traces, such as prompt management, datasets and experiments, have no counterpart on Crucible's
paths; each page lists them and what their calls do against Crucible.
