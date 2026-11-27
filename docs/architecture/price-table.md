# Price table

Stage A of the normalizer prices usage that arrives without a provided cost (contract 3.4). The prices come from
a versioned, immutable price table. The bundled table is compiled into the binary from
`crates/crucible-normalize/data/prices.json`. [Priced models](../guides/priced-models.md) lists its models for
users.

## Version

`BUNDLED_PRICE_TABLE_VERSION` in `crates/crucible-normalize/src/price.rs` is the low 32 bits of the xxh3-64 of the
data file's bytes. Every `BatchSeal` records the version a batch was priced with (contract 5.3), and a table never
changes under a version, so a replay computes identical costs. A test fails until the constant matches the file.

## Format

The file is a JSON array of model rows. An illustrative row (the bundled rows carry no `start`):

```json
{
  "name": "gpt-4o",
  "match": "(openai/)?(gpt-4o)",
  "start": "2024-05-13",
  "tiers": [
    {
      "name": "Fast mode",
      "priority": 1,
      "when": [{ "param": "service_tier", "in": ["fast", "priority"] }],
      "prices": { "input": 4250000, "cache_read": 2125000, "output": 17000000 }
    },
    {
      "name": "Standard",
      "priority": 2,
      "prices": { "input": 2500000, "cache_read": 1250000, "output": 10000000 }
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `name` | A label. `name` and `start` together are unique in the table, and the row id is hashed from them |
| `match` | A regular expression in the syntax of the Rust `regex` crate. It must match the whole model name, ignoring case: the table compiles it as `(?i)^(?:match)$`, so it carries no anchors of its own |
| `start` | Optional `YYYY-MM-DD` (UTC). The row applies to spans that start on or after that day. Without it the row applies from the Unix epoch |
| `tiers[].name` | A label, unique in the row. The tier id is hashed from it and the row's name and start |
| `tiers[].priority` | Tiers are tried in ascending priority. Priorities are unique in the row |
| `tiers[].when` | Conditions that must all hold. Absent or empty means the tier always holds, so only the last tier may omit it |
| `tiers[].prices` | Pico-USD per unit, by usage key: non-negative integers |

A condition is one of:

- `{ "usage": "input", "op": "gt", "value": 200000 }`: the units of a usage key (0 when absent) compared with a
  threshold. `op` is `gt`, `gte`, `lt`, `lte`, `eq` or `neq`.
- `{ "param": "service_tier", "in": ["priority"] }`: a top-level member of the span's `model_parameters` object
  whose value is one of the listed strings. A number or a boolean compares as its JSON text.

## Selection

1. The model name is the span's `model_response`, else its `model_requested`.
2. Project override rows are searched before bundled rows.
3. Among the rows whose pattern matches and whose start is at or before the span's `start_time`, the greatest
   start wins; on a tie the row listed first wins.
4. The first tier by priority whose conditions hold is used. Without usage, a matching row or a holding tier, the
   span has `cost_source = none`.

## Usage keys and parts

Prices use the normalized usage keys of contract 3.1: `input`, `output`, `total`, `cache_read`, `cache_write`,
`reasoning`, and any other key a span provides. `cache_read` and `cache_write` count inside `input`, `reasoning`
inside `output`, and `input` and `output` inside `total` (contract 10.5). Every unit is priced once, by the most
specific key that holds it and has a price:

- A key is priced on its units less the units priced within its parts. A part without a price stays in the key
  that holds it, so a reasoning token is priced as output unless the tier prices `reasoning`.
- Any other key is priced on its own units.
- A key's cost is `round_half_even(units * price / 1000)` nano-USD, exact and saturating at the `i64` maximum. The
  span's `total_cost_nusd` is the sum of the per-key costs.

Leave out a part's price when it equals the price of the key that holds it. A usage threshold on prompt size is a
threshold on `input`, which already includes the cache reads and writes.

## Changing prices

1. Convert a published price to pico-USD per token: USD per million tokens times 1,000,000. For example 2.50 USD
   per million tokens is `2500000`. Prices must convert exactly; there is no fractional pico-USD.
2. Record a price change as a new row with the same `name` and `match` and a `start` on the day the new price
   takes effect. Spans before that day keep the old price. Edit a row in place only to correct a mistake.
3. A new model is a new row. Put rows with more specific patterns first when patterns can overlap.
4. Run `cargo nextest run -p crucible-normalize`. The table loads in a test, which checks every rule above and
   rejects match patterns that are longer than 1,024 bytes, nest groups more than 32 deep or compile too large.
   The version test then prints the new `BUNDLED_PRICE_TABLE_VERSION`; set the constant to it. A new row or tier
   also changes the row and tier counts that `the_bundled_table_loads` asserts.
5. When a new model has names worth pinning, add them with the row they must match to
   `crates/crucible-normalize/tests/fixtures/model_names.json` and update the corpus count in `tests/prices.rs`.
6. A change to the data file changes the price table version, not `NORMALIZER_VERSION`.
7. Update the model names, the version and the date in `docs/guides/priced-models.md`.
