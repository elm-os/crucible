# Priced models

When a span reports token usage and no cost, Crucible prices the usage from a price table built into the server.
This page lists the models that table prices. [Sending traces](sending-traces.md#attributes-the-trace-views-read)
lists the keys that carry a span's model, usage and cost.

## How a span is priced

- The table comes from `crates/crucible-normalize/data/prices.json`. This page lists version `0x582c7ae8` of it,
  last changed on 2026-11-17, with 189 model rows.
- The model is the span's response model (`gen_ai.response.model`), else its requested model
  (`gen_ai.request.model`).
- A row matches the whole model name, ignoring case. `gpt-4o-mini-2024-07-18` and `GPT-4O-MINI` are priced, while
  `gpt-4o-mini-2024-07-19`, a date the table does not list, is not. Neither is a name with a prefix the table does
  not list, such as `models/gemini-3.5-flash` or `openai:gpt-5`.
- Some rows have more than one price. Another price applies when the span's model parameters ask for it, such as
  `service_tier` set to `priority` (from `gen_ai.request.service_tier`, `openai.request.service_tier` or
  `crucible.model_parameters.service_tier`), or when the input is above a size threshold, such as 272,000 tokens.
  Otherwise the row's base price applies.

## Models the table does not list

A model the table does not list gets no cost: the traces and spans lists show $0, the REST API returns
`total_cost_usd` 0, and the span's `cost_source` in `crucible_sql.spans` is `none`. To price such a model, a
self-hosted one for example, send the cost with the span:

- `crucible.cost.total` is the span's cost in USD, as a decimal string or a double.
- `crucible.cost.<usage key>` is the cost of one usage key, such as `crucible.cost.input`. Without
  `crucible.cost.total`, the span's cost is the sum of these.

A provided cost replaces the table's price for that span, and its `cost_source` is `provided`.

## Check what was priced

`cost_source` says where each span's cost came from: `calculated` from the table, `provided` by the span, or `none`.
This query in the [SQL workspace](sql-workspace.md) counts the spans with token usage by model and cost source; a
row with `none` names a model the table does not list, or no model.

```sql
SELECT model_response, model_requested, cost_source, count() AS spans
FROM crucible_sql.spans
WHERE start_time >= {from:DateTime64(6)} AND start_time < {to:DateTime64(6)} AND total_tokens > 0
GROUP BY model_response, model_requested, cost_source
ORDER BY spans DESC
```

## Models

These tables list the names the table prices, by provider and family. The last column gives examples of other
forms that names on the line also match, such as a provider prefix or a cloud platform's form of the name. Other
names on the same line can take a different suffix in such a form, or have no such form. Each row's exact pattern
is its `match` field in the data file, and `cost_source` shows whether a span was priced.

### OpenAI

| Family | Model names | Also matches, for example |
|---|---|---|
| GPT-3.5 | `gpt-3.5-turbo`, `gpt-3.5-turbo-0125`, `gpt-3.5-turbo-0301`, `gpt-3.5-turbo-0613`, `gpt-3.5-turbo-1106`, `gpt-3.5-turbo-16k`, `gpt-3.5-turbo-16k-0613`, `gpt-3.5-turbo-instruct`, `gpt-35-turbo`, `gpt-35-turbo-0125`, `gpt-35-turbo-0301`, `gpt-35-turbo-0613`, `gpt-35-turbo-1106`, `gpt-35-turbo-16k`, `gpt-35-turbo-16k-0613`, `gpt-35-turbo-instruct` | `openai/gpt-3.5-turbo` |
| GPT-4 | `gpt-4`, `gpt-4-0125-preview`, `gpt-4-0314`, `gpt-4-0613`, `gpt-4-1106-preview`, `gpt-4-1106-vision-preview`, `gpt-4-32k`, `gpt-4-32k-0314`, `gpt-4-32k-0613`, `gpt-4-preview`, `gpt-4-turbo`, `gpt-4-turbo-2024-04-09`, `gpt-4-turbo-preview`, `gpt-4-vision-preview` | `openai/gpt-4` |
| GPT-4o | `gpt-4o`, `gpt-4o-2024-05-13`, `gpt-4o-2024-08-06`, `gpt-4o-2024-11-20`, `gpt-4o-mini`, `gpt-4o-mini-2024-07-18` | `openai/gpt-4o` |
| GPT-4o | `chatgpt-4o-latest` |  |
| GPT-4.1 | `gpt-4.1`, `gpt-4.1-2025-04-14`, `gpt-4.1-mini`, `gpt-4.1-mini-2025-04-14`, `gpt-4.1-nano`, `gpt-4.1-nano-2025-04-14` | `openai/gpt-4.1` |
| GPT-4.5 | `gpt-4.5-preview`, `gpt-4.5-preview-2025-02-27` | `openai/gpt-4.5-preview` |
| GPT-5 | `gpt-5`, `gpt-5-2025-08-07`, `gpt-5-chat-latest`, `gpt-5-mini`, `gpt-5-mini-2025-08-07`, `gpt-5-nano`, `gpt-5-nano-2025-08-07`, `gpt-5-pro`, `gpt-5-pro-2025-10-06`, `gpt-5-search-api` | `openai/gpt-5` |
| GPT-5.1 | `gpt-5.1`, `gpt-5.1-2025-11-13` | `openai/gpt-5.1` |
| GPT-5.2 | `gpt-5.2`, `gpt-5.2-2025-12-11`, `gpt-5.2-pro`, `gpt-5.2-pro-2025-12-11` | `openai/gpt-5.2` |
| GPT-5.3 | `gpt-5.3-codex` | `openai/gpt-5.3-codex` |
| GPT-5.4 | `gpt-5.4`, `gpt-5.4-2026-03-05`, `gpt-5.4-mini`, `gpt-5.4-mini-2026-03-17`, `gpt-5.4-nano`, `gpt-5.4-nano-2026-03-17`, `gpt-5.4-pro`, `gpt-5.4-pro-2026-03-05` | `openai/gpt-5.4` |
| GPT-5.5 | `gpt-5.5`, `gpt-5.5-2026-04-23`, `gpt-5.5-pro`, `gpt-5.5-pro-2026-04-23` | `openai/gpt-5.5` |
| GPT-5.6 | `gpt-5.6-cyber`, `gpt-5.6-luna`, `gpt-5.6-sol`, `gpt-5.6-terra` | `openai/gpt-5.6-cyber` |
| GPT-5.6 | `us.openai.gpt-5.6-luna`, `us.openai.gpt-5.6-terra` |  |
| GPT-6 | `gpt-6-astra`, `gpt-6-cyber`, `gpt-6-luna`, `gpt-6-sol` | `openai/gpt-6-astra` |
| GPT-6 | `global.openai.gpt-6-luna`, `us.openai.gpt-6-luna` |  |
| GPT-6.1 | `gpt-6.1-sol`, `gpt-6.1-terra` | `openai/gpt-6.1-sol` |
| o1, o3 and o4 | `o1`, `o1-2024-12-17`, `o1-mini`, `o1-mini-2024-09-12`, `o1-preview`, `o1-preview-2024-09-12`, `o1-pro`, `o1-pro-2025-03-19`, `o3`, `o3-2025-04-16`, `o3-mini`, `o3-mini-2025-01-31`, `o3-pro`, `o3-pro-2025-06-10`, `o4-mini`, `o4-mini-2025-04-16` | `openai/o1` |
| GPT audio | `gpt-audio`, `gpt-audio-1.5`, `gpt-audio-2025-08-28`, `gpt-audio-mini`, `gpt-audio-mini-2025-10-06`, `gpt-audio-mini-2025-12-15` | `openai/gpt-audio` |
| Embeddings | `text-embedding-3-large`, `text-embedding-3-small`, `text-embedding-ada-002`, `text-embedding-ada-002-v2` | `openai/text-embedding-3-large` |
| Completions | `babbage-002`, `davinci-002`, `text-ada-001`, `text-babbage-001`, `text-curie-001`, `text-davinci-001`, `text-davinci-002`, `text-davinci-003` |  |
| Fine-tuned | `ft:gpt-3.5-turbo-1106:`, `ft:gpt-3.5-turbo-0613:`, `ft:davinci-002:` and `ft:babbage-002:`, each followed by the organization, a name and an id, such as `ft:gpt-3.5-turbo-1106:my-org:custom_suffix:id` |  |

### Anthropic

| Family | Model names | Also matches, for example |
|---|---|---|
| Claude 1 and 2 | `claude-1.1`, `claude-1.2`, `claude-1.3`, `claude-2.0`, `claude-2.1`, `claude-instant-1`, `claude-instant-1.2` | `anthropic/claude-1.1` |
| Claude 3 | `claude-3-haiku-20240307`, `claude-3-opus-20240229`, `claude-3-sonnet-20240229` | `anthropic/claude-3-haiku-20240307`, `us.anthropic.claude-3-opus-20240229-v1:0`, `claude-3-opus@20240229` |
| Claude 3.5 | `claude-3-5-haiku-20241022`, `claude-3-5-sonnet-20240620`, `claude-3-5-sonnet-20241022` | `anthropic/claude-3-5-haiku-20241022`, `us.anthropic.claude-3-5-sonnet-20240620-v1:0`, `claude-3-5-sonnet@20240620` |
| Claude 3.5 | `claude-3-5-haiku-latest`, `claude-3-5-sonnet-latest` | `anthropic/claude-3-5-haiku-latest` |
| Claude 3.7 | `claude-3-7-sonnet-20250219` | `anthropic/claude-3-7-sonnet-20250219`, `us.anthropic.claude-3-7-sonnet-20250219-v1:0`, `claude-3-7-sonnet-V1@20250219` |
| Claude 3.7 | `claude-3-7-sonnet-latest` | `anthropic/claude-3-7-sonnet-latest` |
| Claude 4 | `claude-opus-4`, `claude-opus-4-20250514`, `claude-sonnet-4`, `claude-sonnet-4-20250514` | `anthropic/claude-opus-4`, `us.anthropic.claude-sonnet-4-20250514-v1:0`, `claude-sonnet-4@20250514` |
| Claude 4 | `claude-sonnet-4-latest` | `anthropic/claude-sonnet-4-latest` |
| Claude 4.1 | `claude-opus-4-1`, `claude-opus-4-1-20250805` | `anthropic/claude-opus-4-1`, `us.anthropic.claude-opus-4-1-20250805-v1:0`, `claude-opus-4-1@20250805` |
| Claude 4.5 | `claude-haiku-4-5`, `claude-haiku-4-5-20251001`, `claude-opus-4-5`, `claude-opus-4-5-20251101`, `claude-sonnet-4-5`, `claude-sonnet-4-5-20250929` | `anthropic/claude-haiku-4-5`, `us.anthropic.claude-sonnet-4-5-20250929-v1:0`, `claude-sonnet-4-5@20250929` |
| Claude 4.6 | `claude-opus-4-6`, `claude-sonnet-4-6` | `anthropic/claude-opus-4-6`, `us.anthropic.claude-sonnet-4-6-v1:0` |
| Claude 4.7 | `claude-opus-4-7` | `anthropic/claude-opus-4-7`, `us.anthropic.claude-opus-4-7-v1:0` |
| Claude 4.8 | `claude-opus-4-8` | `anthropic/claude-opus-4-8`, `us.anthropic.claude-opus-4-8-v1:0` |
| Claude 5 | `claude-fable-5`, `claude-mythos-5`, `claude-opus-5`, `claude-sonnet-5` | `anthropic/claude-fable-5`, `us.anthropic.claude-fable-5-v1:0` |
| Claude 5.1 | `claude-fable-5-1`, `claude-mythos-5-1` | `anthropic/claude-fable-5-1`, `us.anthropic.claude-fable-5-1-v1:0` |
| Claude 5.2 | `claude-fable-5-2`, `claude-mythos-5-2` | `anthropic/claude-fable-5-2`, `us.anthropic.claude-fable-5-2-v1:0` |
| Claude 5.5 | `claude-haiku-5-5`, `claude-opus-5-5`, `claude-sonnet-5-5` | `anthropic/claude-haiku-5-5`, `us.anthropic.claude-sonnet-5-5-v1:0` |

### Google

| Family | Model names | Also matches, for example |
|---|---|---|
| Gemini 1.0 | `gemini-1.0-pro`, `gemini-1.0-pro-001`, `gemini-1.0-pro-latest` | `google/gemini-1.0-pro`, `gemini-1.0-pro-latest@001` |
| Gemini 1.5 | `gemini-1.5-pro-latest` | `google/gemini-1.5-pro-latest`, `gemini-1.5-pro-latest@001` |
| Gemini 2.0 | `gemini-2.0-flash`, `gemini-2.0-flash-001`, `gemini-2.0-flash-lite-preview`, `gemini-2.0-flash-lite-preview-02-05` | `google/gemini-2.0-flash`, `gemini-2.0-flash-001@001` |
| Gemini 2.5 | `gemini-2.5-flash`, `gemini-2.5-flash-lite`, `gemini-2.5-pro` | `google/gemini-2.5-flash` |
| Gemini 3 | `gemini-3-flash-preview`, `gemini-3-pro-preview` | `google/gemini-3-flash-preview` |
| Gemini 3.1 | `gemini-3.1-flash-lite`, `gemini-3.1-flash-lite-preview`, `gemini-3.1-pro-preview`, `gemini-3.1-pro-preview-customtools` | `google/gemini-3.1-flash-lite` |
| Gemini 3.5 | `gemini-3.5-flash`, `gemini-3.5-flash-lite` | `google/gemini-3.5-flash` |
| Gemini 3.6 | `gemini-3.6-flash` | `google/gemini-3.6-flash` |
| Gemini 3.7 | `gemini-3.7-flash` | `google/gemini-3.7-flash` |
| Gemini 3.8 | `gemini-3.8-flash` | `google/gemini-3.8-flash` |
| Gemini 4 | `gemini-4-argon` | `google/gemini-4-argon` |
| Other Gemini names | `gemini-pro` | `google/gemini-pro`, `gemini-pro@001` |
| Other Gemini names | `gemini-decision-1` | `google/gemini-decision-1` |
| Bison, Gecko and Unicorn | `chat-bison`, `chat-bison-32k`, `code-bison`, `code-bison-32k`, `code-gecko`, `codechat-bison`, `codechat-bison-32k`, `text-bison`, `text-bison-32k`, `text-unicorn`, `textembedding-gecko`, `textembedding-gecko-multilingual` | `chat-bison@001` |

### Cohere

| Family | Model names | Also matches, for example |
|---|---|---|
| Embed v4 | `cohere.embed-v4:0`, `eu.cohere.embed-v4:0`, `us.cohere.embed-v4:0` |  |

### Other

| Family | Model names | Also matches, for example |
|---|---|---|
| jev | `jev`, `jev-latest`, `jev-preview`, and `jev-` with a version, such as `jev-1.13.0` or `jev-1.13-20260917` | `typesafe/jev` |
