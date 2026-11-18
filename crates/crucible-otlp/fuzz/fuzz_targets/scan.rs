//! Fuzzes the protobuf scan: it never panics, it refuses every body prost refuses, and every body it
//! accepts reads through the views exactly as prost decodes it.
//!
//! Run from `crates/crucible-otlp/fuzz`: `cargo +nightly fuzz run scan -- -dict=dict/otlp.dict`.
#![no_main]

use crucible_otlp::TraceRequest;
use libfuzzer_sys::fuzz_target;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use prost::Message;

#[path = "../../tests/common/oracle.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let decoded = ExportTraceServiceRequest::decode(data);
    if let Ok(scanned) = TraceRequest::scan(data, None) {
        let decoded = decoded.expect("the scan accepted a body prost refuses");
        assert_eq!(oracle::observe(&scanned), oracle::expect(&decoded, None));
    }
});
