//! The compat fixtures (contract 10): one OTLP/JSON export request per file, with the fields the mapping must
//! give. Every value is invented; the shapes (keys, encodings, nesting and wire order) are the ones each SDK
//! sends, and each file names where its shape is documented under `provenance`.
//!
//! One file is `{ "provenance", "request": { "path", "headers" }, "otlp": <OTLP JSON export request>,
//! "expect": { "<span id hex>": { ...fields } } }`.

/// One fixture file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fixture {
    /// The dialect the fixture belongs to (its directory).
    pub dialect: u8,
    /// The file name without its extension.
    pub id: &'static str,
    /// The JSON text of the file.
    pub json: &'static str,
}

/// Every fixture, by dialect and then by name.
pub static FIXTURES: &[Fixture] = &[
    Fixture {
        dialect: 1,
        id: "f1-server-sdk-legacy",
        json: include_str!("../fixtures/d1/f1-server-sdk-legacy.json"),
    },
    Fixture {
        dialect: 1,
        id: "f2-python-v5-openai",
        json: include_str!("../fixtures/d1/f2-python-v5-openai.json"),
    },
    Fixture {
        dialect: 1,
        id: "f3-js-v6-propagated",
        json: include_str!("../fixtures/d1/f3-js-v6-propagated.json"),
    },
    Fixture {
        dialect: 1,
        id: "s1-disjoint-usage",
        json: include_str!("../fixtures/d1/s1-disjoint-usage.json"),
    },
    Fixture {
        dialect: 1,
        id: "s2-genai-on-path",
        json: include_str!("../fixtures/d1/s2-genai-on-path.json"),
    },
    Fixture {
        dialect: 1,
        id: "s3-forwarded-foreign",
        json: include_str!("../fixtures/d1/s3-forwarded-foreign.json"),
    },
    Fixture {
        dialect: 1,
        id: "s4-fanout-and-media",
        json: include_str!("../fixtures/d1/s4-fanout-and-media.json"),
    },
    Fixture {
        dialect: 2,
        id: "f1-js-openai-cache",
        json: include_str!("../fixtures/d2/f1-js-openai-cache.json"),
    },
    Fixture {
        dialect: 2,
        id: "f2-js-openai-tool-loop",
        json: include_str!("../fixtures/d2/f2-js-openai-tool-loop.json"),
    },
    Fixture {
        dialect: 2,
        id: "f3-js-decision-context",
        json: include_str!("../fixtures/d2/f3-js-decision-context.json"),
    },
    Fixture {
        dialect: 2,
        id: "s1-dual-written",
        json: include_str!("../fixtures/d2/s1-dual-written.json"),
    },
    Fixture {
        dialect: 3,
        id: "f1-legacy-openai-chat",
        json: include_str!("../fixtures/d3/f1-legacy-openai-chat.json"),
    },
    Fixture {
        dialect: 3,
        id: "f2-current-openai-chat",
        json: include_str!("../fixtures/d3/f2-current-openai-chat.json"),
    },
    Fixture {
        dialect: 3,
        id: "f3-sdk-task-association",
        json: include_str!("../fixtures/d3/f3-sdk-task-association.json"),
    },
    Fixture {
        dialect: 4,
        id: "f1-v6-generate-text",
        json: include_str!("../fixtures/d4/f1-v6-generate-text.json"),
    },
    Fixture {
        dialect: 4,
        id: "f2-v6-stream-text-tools",
        json: include_str!("../fixtures/d4/f2-v6-stream-text-tools.json"),
    },
    Fixture {
        dialect: 4,
        id: "f3-v7-legacy-stream-text",
        json: include_str!("../fixtures/d4/f3-v7-legacy-stream-text.json"),
    },
    Fixture {
        dialect: 4,
        id: "s1-v7-default-class",
        json: include_str!("../fixtures/d4/s1-v7-default-class.json"),
    },
    Fixture {
        dialect: 5,
        id: "f1-chat-usage",
        json: include_str!("../fixtures/d5/f1-chat-usage.json"),
    },
    Fixture {
        dialect: 5,
        id: "f2-tool-io",
        json: include_str!("../fixtures/d5/f2-tool-io.json"),
    },
    Fixture {
        dialect: 5,
        id: "f3-chat-openai-inputs",
        json: include_str!("../fixtures/d5/f3-chat-openai-inputs.json"),
    },
    Fixture {
        dialect: 5,
        id: "s1-levels-prompts-tags",
        json: include_str!("../fixtures/d5/s1-levels-prompts-tags.json"),
    },
    Fixture {
        dialect: 5,
        id: "s2-foreign-stamped",
        json: include_str!("../fixtures/d5/s2-foreign-stamped.json"),
    },
    Fixture {
        dialect: 5,
        id: "s3-quoted-semconv",
        json: include_str!("../fixtures/d5/s3-quoted-semconv.json"),
    },
    Fixture {
        dialect: 6,
        id: "f1-chat-cost-cache",
        json: include_str!("../fixtures/d6/f1-chat-cost-cache.json"),
    },
    Fixture {
        dialect: 6,
        id: "f2-agent-run",
        json: include_str!("../fixtures/d6/f2-agent-run.json"),
    },
    Fixture {
        dialect: 6,
        id: "f3-tool-call",
        json: include_str!("../fixtures/d6/f3-tool-call.json"),
    },
    Fixture {
        dialect: 6,
        id: "s1-v1-agent-cumulative",
        json: include_str!("../fixtures/d6/s1-v1-agent-cumulative.json"),
    },
    Fixture {
        dialect: 7,
        id: "f1-go-eval-case",
        json: include_str!("../fixtures/d7/f1-go-eval-case.json"),
    },
    Fixture {
        dialect: 7,
        id: "f2-dotnet-flattened-metrics",
        json: include_str!("../fixtures/d7/f2-dotnet-flattened-metrics.json"),
    },
    Fixture {
        dialect: 7,
        id: "f3-js-forwarded-span",
        json: include_str!("../fixtures/d7/f3-js-forwarded-span.json"),
    },
    Fixture {
        dialect: 7,
        id: "s1-metrics-vectors",
        json: include_str!("../fixtures/d7/s1-metrics-vectors.json"),
    },
    Fixture {
        dialect: 7,
        id: "s2-system-keys-only",
        json: include_str!("../fixtures/d7/s2-system-keys-only.json"),
    },
    Fixture {
        dialect: 7,
        id: "s3-flattened-messages",
        json: include_str!("../fixtures/d7/s3-flattened-messages.json"),
    },
];
