//! Fuzzes the OTLP/JSON transcoder: it never panics, whatever it accepts is protobuf that prost
//! decodes, and when that protobuf scans, the views read exactly as prost decodes it.
//!
//! Run from `crates/crucible-otlp/fuzz`: `cargo +nightly fuzz run json -- -dict=dict/otlp.dict`.
#![no_main]

use crucible_otlp::{TraceRequest, transcode_json};
use libfuzzer_sys::fuzz_target;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use prost::Message;

#[path = "../../tests/common/oracle.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let Ok(bytes) = transcode_json(data) else {
        return;
    };
    let decoded = ExportTraceServiceRequest::decode(bytes.as_slice())
        .expect("the transcoder wrote protobuf that prost refuses");
    if let Ok(scanned) = TraceRequest::scan(&bytes, None) {
        assert_eq!(oracle::observe(&scanned), oracle::expect(&decoded, None));
    }
});
