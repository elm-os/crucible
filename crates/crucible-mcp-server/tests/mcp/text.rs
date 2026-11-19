//! The text tools: search over inputs, outputs and metadata entries, and payload pieces that read a whole field
//! under the result cap (contract 8.7 and 12.4).

use std::sync::Arc;

use crucible_recent::SpanEntry;
use crucible_records::TableId;
use serde_json::{Value, json};

use crate::caps::hostile;
use crate::world::{NOW, World, span, span_payload_row, trace};
use crate::{Client, key, mcp, run, window};

const MINUTE: i64 = 60_000_000;
const DAY: i64 = 86_400_000_000;

/// Trace 1's spans in the store, one per input with its metadata entries, a minute apart, the first the newest.
fn world_with(inputs: &[(&str, &[&str])]) -> (World, Vec<SpanEntry>) {
    let mut world = World::new(1_000);
    let entries: Vec<SpanEntry> = (0..inputs.len())
        .map(|i| {
            let start = NOW.saturating_add_micros(-(i as i64 + 1) * MINUTE);
            span(1, 10 + i as u64, 0, start)
        })
        .collect();
    let lsn = world.ack(entries.clone());
    for (entry, (input, metadata)) in entries.iter().zip(inputs) {
        let row = span_payload_row(entry, lsn, NOW, input, metadata);
        world.store.insert(TableId::SpanPayload, row);
    }
    world.settle();
    (world, entries)
}

fn search(query: &str) -> Value {
    let mut arguments = window(NOW.saturating_add_micros(-DAY), NOW);
    arguments["query"] = json!(query);
    arguments
}

fn span_ids(page: &Value) -> Vec<&str> {
    page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["span"]["span_id"].as_str().unwrap())
        .collect()
}

#[test]
fn search_finds_spans_by_their_words_and_their_metadata_entries() {
    run(|env| async move {
        let (world, _) = world_with(&[
            ("The refund was declined", &["tier=gold"]),
            ("refund approved", &[]),
            ("nothing to see", &["tier=free"]),
        ]);
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));
        let found = client.output("search_spans", search("REFUND")).await;
        // Newest first, each hit with its trace and the word highlighted.
        assert_eq!(span_ids(&found), ["000000000000000a", "000000000000000b"]);
        let hit = &found["data"][0];
        assert_eq!(hit["trace_id"], trace(1).to_string());
        assert_eq!(hit["span"]["input_preview"], "preview");
        let fragments = hit["matches"][0]["fragments"].as_array().unwrap();
        let marked: Vec<&Value> = fragments
            .iter()
            .filter(|f| f["highlighted"] == true)
            .collect();
        assert_eq!(marked[0]["text"], "refund");
        assert_eq!(found["has_more"], false);
        insta::assert_snapshot!(
            "search_spans",
            serde_json::to_string_pretty(&found).unwrap()
        );

        let mut metadata = search("tier=gold");
        metadata["fields"] = json!(["metadata"]);
        let found = client.output("search_spans", metadata).await;
        assert_eq!(span_ids(&found), ["000000000000000a"]);
        let mut any = search("declined approved");
        any["mode"] = json!("any");
        assert_eq!(span_ids(&client.output("search_spans", any).await).len(), 2);
        let mut phrase = search("refund was");
        phrase["mode"] = json!("phrase");
        assert_eq!(
            span_ids(&client.output("search_spans", phrase).await),
            ["000000000000000a"]
        );

        // A page of one hit continues with its cursor, which only that search opens.
        let mut paged = search("refund");
        paged["limit"] = json!(1);
        let first = client.output("search_spans", paged.clone()).await;
        assert_eq!(span_ids(&first), ["000000000000000a"]);
        paged["cursor"] = first["next_cursor"].clone();
        let second = client.output("search_spans", paged.clone()).await;
        assert_eq!(span_ids(&second), ["000000000000000b"]);
        paged["query"] = json!("declined");
        let problem = client.error("search_spans", paged).await;
        assert_eq!(problem["code"], "cursor_invalid");
    });
}

#[test]
fn a_search_too_large_for_one_result_is_cut_and_continues_newest_first() {
    run(|env| async move {
        // 30 hits in trace 1, three to a minute, each named with 1,024 bytes that JSON escapes heavily, so a
        // result holds a few hits and the default page of 20 is cut.
        let mut world = World::new(10_000);
        let entries: Vec<SpanEntry> = (0..30u64)
            .map(|i| {
                let minutes = i64::try_from(i / 3).unwrap() + 1;
                let start =
                    NOW.saturating_add_micros(-minutes * MINUTE - i64::try_from(i).unwrap());
                let mut entry = span(1, 100 + i, 0, start);
                entry.summary.name = Arc::from(hostile(i));
                entry
            })
            .collect();
        let lsn = world.ack(entries.clone());
        for entry in &entries {
            let row = span_payload_row(entry, lsn, NOW, "refund requested", &[]);
            world.store.insert(TableId::SpanPayload, row);
        }
        world.settle();
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));

        let mut arguments = search("refund");
        let (mut read, mut cut) = (Vec::new(), 0);
        loop {
            let page = client.output("search_spans", arguments.clone()).await;
            if page["truncated"] == true {
                cut += 1;
            }
            read.extend(span_ids(&page).into_iter().map(str::to_owned));
            match page["next_cursor"].as_str() {
                Some(cursor) => arguments["cursor"] = json!(cursor),
                None => break,
            }
        }
        assert!(cut > 1, "{cut} cut pages");
        // Every hit once, newest first along the hits' key: the start minute, then the span id.
        let mut expected: Vec<_> = entries
            .iter()
            .map(|e| (e.summary.start_time.floor_minute(), e.summary.span_id))
            .collect();
        expected.sort_unstable_by(|a, b| b.cmp(a));
        let expected: Vec<String> = expected.iter().map(|(_, id)| id.to_string()).collect();
        assert_eq!(read, expected);
    });
}

#[test]
fn search_arguments_are_checked_like_the_rest_search() {
    run(|env| async move {
        let (world, _) = world_with(&[("refund", &[])]);
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));
        let long = "word ".repeat(300);
        let cases = [
            (json!({ "query": "!!" }), "/query", "no_tokens"),
            (json!({ "query": long }), "/query", "too_long"),
            (json!({ "query": "a", "fields": [] }), "/fields", "required"),
            (
                json!({ "query": "a", "fields": ["input", "input"] }),
                "/fields",
                "repeated",
            ),
            (
                json!({ "query": "words", "fields": ["metadata"] }),
                "/query",
                "no_tokens",
            ),
            (
                json!({ "query": "a", "limit": 21 }),
                "/limit",
                "out_of_range",
            ),
        ];
        for (extra, pointer, code) in cases {
            let mut arguments = search("");
            for (name, value) in extra.as_object().unwrap() {
                arguments[name] = value.clone();
            }
            let problem = client.error("search_spans", arguments.clone()).await;
            assert_eq!(problem["code"], "validation_failed", "{arguments}");
            assert_eq!(problem["errors"][0]["pointer"], pointer, "{problem:#}");
            assert_eq!(problem["errors"][0]["code"], code, "{problem:#}");
        }
    });
}

#[test]
fn a_payload_is_read_whole_in_pieces_under_the_result_cap() {
    run(|env| async move {
        // About 94 KiB that JSON escapes heavily: quotes, backslashes, newlines and two- and three-byte
        // characters.
        let input: String = (0..60_000)
            .map(|i| ["\"", "\\", "\n", "é", "語"][i % 5])
            .collect();
        let (world, entries) = world_with(&[(&input, &[])]);
        let mcp = mcp(&env, &world);
        let mut client = Client::modern(&mcp, &key(1));
        let mut arguments = json!({
            "trace_id": trace(1).to_string(),
            "span_id": "000000000000000a",
            "start_time": entries[0].summary.start_time.to_string(),
            "field": "input",
        });
        let (mut read, mut calls) = (String::new(), 0);
        loop {
            let piece = client.output("get_span_payload", arguments.clone()).await;
            calls += 1;
            assert_eq!(piece["offset"], read.len(), "{piece:#}");
            assert_eq!(piece["total_bytes"], input.len());
            let text = piece["text"].as_str().unwrap();
            assert!(text.len() <= 32 * 1024);
            read.push_str(text);
            match piece["next_offset"].as_u64() {
                Some(next) => {
                    assert_eq!(piece["truncated"], true);
                    arguments["offset"] = json!(next);
                }
                None => break,
            }
        }
        assert_eq!(read, input);
        assert!(calls > 3, "{calls} calls");

        // Another field of the same span is empty; another project's key finds nothing.
        arguments["field"] = json!("output");
        arguments["offset"] = json!(0);
        let output = client.output("get_span_payload", arguments.clone()).await;
        assert_eq!(
            (output["text"].as_str(), output["next_offset"].as_u64()),
            (Some(""), None)
        );
        let problem = Client::modern(&mcp, &key(2))
            .error("get_span_payload", arguments.clone())
            .await;
        assert_eq!(problem["code"], "not_found", "{problem:#}");
        arguments["field"] = json!("metadata");
        let problem = client.error("get_span_payload", arguments.clone()).await;
        assert_eq!(problem["code"], "validation_failed");
        arguments["field"] = json!("input");
        arguments["start_time"] = json!("yesterday");
        let problem = client.error("get_span_payload", arguments).await;
        assert_eq!(problem["errors"][0]["pointer"], "/start_time");
    });
}
