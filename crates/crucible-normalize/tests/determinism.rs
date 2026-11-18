//! Replay determinism (contracts 3.2 and 5.3): the same frame and the same `NormCtx` give byte-identical records,
//! a replay that loads the price table of the pinned `price_table_version` again computes identical costs, and a
//! span re-sent in another frame (new LSN, epoch or key) has the same row digest, which is what lets the duplicate
//! window drop it.

// Test code throughout: lets the helpers below use `unwrap` under the workspace lints.
#![cfg(test)]

mod common;

use std::sync::LazyLock;

use common::{ctx, frame, generate, int, prices, request, s, span_with};
use crucible_normalize::{BUNDLED_PRICE_TABLE_VERSION, NormCtx, PriceTable, normalize, row_digest};
use crucible_records::{ApiKeyId, CostSource, Lsn, PayloadCaps, WalEpoch};
use proptest::collection::vec;
use proptest::prelude::*;
use prost::Message;

/// The bundled table loaded a second time, from the same file at the same version, as a replay would load it.
static REPLAYED: LazyLock<PriceTable> = LazyLock::new(|| {
    PriceTable::from_json(
        BUNDLED_PRICE_TABLE_VERSION,
        include_str!("../data/prices.json"),
    )
    .unwrap()
});

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn a_pinned_price_table_version_prices_identically(request in generate::request(false)) {
        let body = request.encode_to_vec();
        let f = frame(&body, &[]);
        let first = normalize(&f, &ctx()).unwrap();
        let replay_ctx = NormCtx::new(&REPLAYED, PayloadCaps::DEFAULT);
        prop_assert_eq!(replay_ctx.price_table_version, ctx().price_table_version);
        prop_assert_eq!(first, normalize(&f, &replay_ctx).unwrap());
    }

    #[test]
    fn renormalizing_a_frame_gives_identical_records(
        request in generate::request(false),
        rejected in vec(any::<u8>(), 0..3),
        max_field in 0usize..4_096,
        max_span in 0usize..8_192,
    ) {
        let body = request.encode_to_vec();
        let ctx = NormCtx::new(prices(), PayloadCaps { max_field_bytes: max_field, max_span_bytes: max_span });
        let f = frame(&body, &rejected);
        let first = normalize(&f, &ctx).unwrap();
        let second = normalize(&f, &ctx).unwrap();
        prop_assert_eq!(&first, &second);
        for record in &first.spans {
            prop_assert_eq!(record.core.row_digest, row_digest(record));
        }
    }

    /// Only the ingestion group depends on where the frame sits in the WAL and which key sent it.
    #[test]
    fn a_retry_in_another_frame_keeps_its_digests(request in generate::request(false)) {
        let body = request.encode_to_vec();
        let ctx = ctx();
        let original = normalize(&frame(&body, &[]), &ctx).unwrap();
        let mut retry_frame = frame(&body, &[]);
        retry_frame.lsn = Lsn::new(9_999);
        retry_frame.wal_epoch = WalEpoch::from_bytes([0x44; 16]);
        retry_frame.api_key_id = ApiKeyId::from_bytes([0x55; 16]);
        let mut retry = normalize(&retry_frame, &ctx).unwrap();
        prop_assert_eq!(retry.lsn, Lsn::new(9_999));
        for (a, b) in original.spans.iter().zip(&mut retry.spans) {
            prop_assert_eq!(a.core.row_digest, b.core.row_digest);
            prop_assert_eq!(b.core.ingestion.lsn, Lsn::new(9_999));
            b.core.ingestion = a.core.ingestion;
            b.core.api_key_id = a.core.api_key_id;
        }
        prop_assert_eq!(original.spans, retry.spans);
    }
}

#[test]
fn the_context_pins_the_bundled_price_table_version() {
    assert_eq!(ctx().price_table_version, BUNDLED_PRICE_TABLE_VERSION);
    assert_eq!(prices().version(), BUNDLED_PRICE_TABLE_VERSION);
    let body = request(vec![span_with(
        1,
        1,
        None,
        vec![
            s("gen_ai.request.model", "gpt-4o"),
            int("gen_ai.usage.input_tokens", 1_000),
        ],
    )]);
    let f = frame(&body, &[]);
    let first = normalize(&f, &ctx()).unwrap();
    assert_eq!(first.spans[0].core.cost_source, CostSource::Calculated);
    let replay = normalize(&f, &NormCtx::new(&REPLAYED, PayloadCaps::DEFAULT)).unwrap();
    assert_eq!(first, replay);
}

#[test]
fn stamping_the_batch_keeps_every_digest() {
    let body = request(vec![
        span_with(1, 1, None, vec![s("user.id", "u")]),
        span_with(1, 2, Some(1), Vec::new()),
    ]);
    let mut out = normalize(&frame(&body, &[]), &ctx()).unwrap();
    let digests: Vec<u64> = out.spans.iter().map(|r| r.core.row_digest).collect();
    assert!(
        out.spans
            .iter()
            .all(|r| r.core.ingestion.batch_seq == Lsn::ZERO)
    );
    out.stamp_batch_seq(Lsn::new(5));
    for (record, digest) in out.spans.iter().zip(digests) {
        assert_eq!(record.core.ingestion.batch_seq, Lsn::new(5));
        assert_eq!(row_digest(record), digest);
        assert_eq!(record.core.row_digest, digest);
    }
}

#[test]
fn the_context_pins_the_version_and_the_caps() {
    let body = request(vec![span_with(
        1,
        1,
        None,
        vec![s("gen_ai.input.messages", "abcdefgh")],
    )]);
    let f = frame(&body, &[]);
    let base = normalize(&f, &ctx()).unwrap();
    assert_eq!(
        base.spans[0].core.normalizer_version,
        crucible_normalize::NORMALIZER_VERSION
    );
    let mut other_version = ctx();
    other_version.normalizer_version += 1;
    let bumped = normalize(&f, &other_version).unwrap();
    assert_eq!(
        bumped.spans[0].core.normalizer_version,
        base.spans[0].core.normalizer_version + 1
    );
    assert_ne!(
        bumped.spans[0].core.row_digest,
        base.spans[0].core.row_digest
    );
    let capped = normalize(
        &f,
        &NormCtx::new(
            prices(),
            PayloadCaps {
                max_field_bytes: 4,
                max_span_bytes: 1 << 20,
            },
        ),
    )
    .unwrap();
    assert_eq!(&capped.spans[0].payload.input[..], b"abcd");
    assert_ne!(
        capped.spans[0].core.row_digest,
        base.spans[0].core.row_digest
    );
}
