# Moving from Braintrust

Spans that carry Braintrust's OpenTelemetry attributes move to Crucible by changing their export target and key;
Crucible takes them on `/otel/v1/traces`. Braintrust features that run on its REST API rather than on exported
spans, such as experiments, datasets and prompts, are not served by Crucible.

Verified with a plain OpenTelemetry OTLP exporter (Python 1.45.2), braintrust 0.52.0 (Python,
`BraintrustSpanProcessor`) and braintrust-sdk-go 0.20.0.

## What changes

From an OTLP exporter, point the traces endpoint at Crucible's `/otel/v1/traces` path and send a Crucible secret
key, with the variables for traces, which leave metrics and logs where they go today:

```sh
export OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:4318/otel/v1/traces
export OTEL_EXPORTER_OTLP_TRACES_HEADERS="Authorization=Bearer%20csk_...,x-bt-parent=project_name:my-project"
```

`x-bt-parent` can stay or go; the key decides the project.

From the Python SDK's span processor, point its API URL at Crucible, without a path, and give it the key:

```sh
export BRAINTRUST_API_URL=http://127.0.0.1:4318
export BRAINTRUST_API_KEY=csk_...
export BRAINTRUST_PARENT=project_name:my-project
```

From the Go SDK, set the app URL too:

```sh
export BRAINTRUST_API_URL=http://127.0.0.1:4318
export BRAINTRUST_APP_URL=http://127.0.0.1:4318
export BRAINTRUST_API_KEY=csk_...
```

The Go SDK logs in to its app URL at startup: in braintrust-sdk-go 0.20.0, its `internal/auth` package posts to
`/api/apikey/login` on the app URL with a client that carries the API key, and the app URL defaults to
`https://www.braintrust.dev`. In the verification run, with only the API URL changed, the SDK opened connections to
`www.braintrust.dev`, so your Crucible secret key would go there. With both set, the login answers 404, the SDK logs
a warning, and its spans export to Crucible.

These steps were run with the SDKs above. Before pointing another Braintrust SDK at Crucible, check which hosts it
contacts with the key.

Only spans exported after the switch reach Crucible.

## What carries over

Inputs and outputs, metadata, token metrics, span types (scorers become evaluator spans), span tags, the requested
and responding models, the provider, the log level, tool definitions, the prompt version and the time to first
token. [The Braintrust integration page](../integrations/braintrust.md) maps each attribute to its Crucible field.

## What does not carry over

- `braintrust.scores` stays among each span's `attributes` and does not become a Crucible score. For feedback scores,
  use Crucible's feedback route, `POST /api/v1/projects/{project_id}/feedback`
  ([tags, flags, scores and corrections](../guides/facts.md)).
- `braintrust.expected`, `braintrust.expected_json`, `braintrust.parent` and `braintrust.context_json` stay among the
  `attributes` too, and the metrics' `estimated_cost` is not read: cost comes from Crucible's price table.
- Tags stay on the span that carries them; the trace list shows no tags for them.
- OTLP logs are not ingested: `/otel/v1/logs` answers 404.
- Features that use Braintrust's REST API instead of OTLP traces: the SDK's own logger (`init_logger`, `traced` and
  the client wrappers), `Eval()` and experiments, datasets, prompts, online scoring and feedback logging. Their
  requests reach no Crucible route and are refused.
