//! OTLP/gRPC trace ingest (contracts 4.8 and 12.1): `TraceService/Export` through tonic with a raw-bytes
//! codec, so that the message takes the admission, scan, envelope and ack path of `POST /v1/traces`, with the
//! envelope's transport set to gRPC.
//!
//! A request is authenticated and admitted from its metadata before tonic reads its body. While tonic reads,
//! each body frame grows the request's in-flight reservation, so a gRPC request holds the budget from its
//! first byte, as an HTTP request does. tonic inflates gzip messages under the message cap, which is the
//! canonical body cap of contract 4.3.
//!
//! The method is unary on the wire. The server reads it through tonic's client-streaming entry point, so that
//! size and decoding failures come back to this module, which answers them with `INVALID_ARGUMENT` as
//! contract 4.8 asks. tonic's own codes for them are `OUT_OF_RANGE` and `RESOURCE_EXHAUSTED`, which OTLP
//! exporters would retry.
//!
//! A request that is not gRPC, such as an OTLP/HTTP export sent to the gRPC port, gets 415. A gRPC status
//! travels on HTTP 200, which an HTTP exporter would take for success.

use std::sync::{Arc, Mutex, PoisonError};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Response};
use bytes::{Buf as _, BufMut as _};
use crucible_auth::KeyPrincipal;
use crucible_otlp::MAX_BODY_BYTES;
use crucible_wal_codec::IngestTransport;
use futures::StreamExt as _;
use tonic::codec::{Codec, CompressionEncoding, DecodeBuf, Decoder, EncodeBuf, Encoder, Streaming};
use tonic::server::Grpc;
use tonic::{Code, Status};

use crate::admission::Held;
use crate::body::{self, Format};
use crate::reply::{self, Reject};
use crate::service::{Acked, Ingest, Inner, Job, Origin};

/// The path of the `opentelemetry.proto.collector.trace.v1.TraceService/Export` method.
pub(crate) const EXPORT_PATH: &str = "/opentelemetry.proto.collector.trace.v1.TraceService/Export";

/// The most body bytes read for one request: a message at the cap and its 5-byte prefix (the compression flag
/// and the length).
const MAX_READ: u64 = MAX_BODY_BYTES as u64 + 5;

/// Message bytes as they are. The request message is the canonical `ExportTraceServiceRequest`, which the
/// scanner validates; the response message is the one [`reply::export_response`] encodes.
#[derive(Debug, Clone, Copy)]
struct RawCodec;

impl Codec for RawCodec {
    type Encode = Vec<u8>;
    type Decode = Vec<u8>;
    type Encoder = Self;
    type Decoder = Self;

    fn encoder(&mut self) -> Self {
        Self
    }

    fn decoder(&mut self) -> Self {
        Self
    }
}

impl Encoder for RawCodec {
    type Item = Vec<u8>;
    type Error = Status;

    fn encode(&mut self, item: Vec<u8>, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        dst.put_slice(&item);
        Ok(())
    }
}

impl Decoder for RawCodec {
    type Item = Vec<u8>;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Vec<u8>>, Status> {
        let mut message = vec![0; src.remaining()];
        src.copy_to_slice(&mut message);
        Ok(Some(message))
    }
}

/// A request's reservation while tonic reads its body (contract 4.8): every data frame grows it by the frame's
/// bytes, and the body fails once the budget cannot hold them or they pass [`MAX_READ`]. The refusal is kept
/// here, so that the request is answered with it rather than with the read error that tonic reports.
#[derive(Debug)]
struct Meter {
    held: Option<Held>,
    read: u64,
    refused: Option<Reject>,
}

/// The body was cut off because the meter refused the request.
#[derive(Debug, thiserror::Error)]
#[error("the request was refused while its body was read")]
struct Refused;

impl Meter {
    fn charge(&mut self, len: usize) -> Result<(), Refused> {
        self.read = self.read.saturating_add(len as u64);
        let read = self.read;
        let refusal = if read > MAX_READ {
            Some(body::too_large("the request message", MAX_BODY_BYTES))
        } else {
            self.held.as_mut().and_then(|held| held.grow_to(read).err())
        };
        match refusal {
            None => Ok(()),
            Some(reject) => {
                self.refused = Some(reject);
                Err(Refused)
            }
        }
    }
}

/// `body`, charged to `meter` frame by frame as it is read.
fn metered(body: Body, meter: Arc<Mutex<Meter>>) -> Body {
    Body::from_stream(body.into_data_stream().map(move |chunk| {
        let data = chunk?;
        meter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .charge(data.len())
            .map_err(axum::Error::new)?;
        Ok::<_, axum::Error>(data)
    }))
}

/// The refusal for a message that tonic could not read: over the cap, by its length prefix or once inflated,
/// or not decodable at all. Both are `INVALID_ARGUMENT` (contract 4.8).
fn decode_reject(status: &Status) -> Reject {
    match status.code() {
        Code::OutOfRange | Code::ResourceExhausted => body::too_large(
            "the request message (inflated, when it is compressed)",
            MAX_BODY_BYTES,
        ),
        _ => Reject::Malformed(format!(
            "the request message could not be read: {}",
            status.message()
        )),
    }
}

/// Whether a request is gRPC: its media type is `application/grpc`, alone or with a `+` subtype.
fn is_grpc(headers: &HeaderMap) -> bool {
    let value = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let media = value.split(';').next().unwrap_or_default().trim();
    let base = media.split_once('+').map_or(media, |(base, _)| base);
    base.eq_ignore_ascii_case("application/grpc")
}

/// The answer to a request on the gRPC listener that is not gRPC: 415 with where OTLP/HTTP goes.
fn not_grpc() -> Response {
    (
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "this listener takes OTLP over gRPC (Content-Type: application/grpc); send OTLP over HTTP to \
         POST /v1/traces on the HTTP ingest listener\n",
    )
        .into_response()
}

/// `TraceService/Export`.
pub(crate) async fn export(State(ingest): State<Ingest>, request: Request) -> Response {
    let inner = Arc::clone(&ingest.inner);
    if !is_grpc(request.headers()) {
        inner
            .metrics
            .refused(&Reject::UnsupportedMediaType(String::new()));
        return not_grpc();
    }
    let (parts, body) = request.into_parts();
    let admitted = inner.authenticate(&parts.headers).and_then(|principal| {
        let held = inner.admission.admit(principal.scope.project_id(), 0)?;
        Ok((principal, held))
    });
    let (principal, held) = match admitted {
        Ok(admitted) => admitted,
        Err(reject) => {
            inner.metrics.refused(&reject);
            return reject.grpc_status().into_http();
        }
    };
    // Routing headers set a hint on `/v1/traces` only (contract 12.1), so a gRPC export carries none.
    let origin = Origin::of(&parts.headers, None, &[]);
    let meter = Arc::new(Mutex::new(Meter {
        held: Some(held),
        read: 0,
        refused: None,
    }));
    let request = Request::from_parts(parts, metered(body, Arc::clone(&meter)));
    let service = tower::service_fn(move |request: tonic::Request<Streaming<Vec<u8>>>| {
        let (inner, origin, meter) = (Arc::clone(&inner), origin.clone(), Arc::clone(&meter));
        async move {
            inner
                .grpc_export(principal, origin, &meter, request.into_inner())
                .await
        }
    });
    let mut grpc = Grpc::new(RawCodec)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_BODY_BYTES);
    grpc.client_streaming(service, request).await.map(Body::new)
}

/// Any other path: `UNIMPLEMENTED` for a gRPC request.
pub(crate) async fn unimplemented(request: Request) -> Response {
    if !is_grpc(request.headers()) {
        return not_grpc();
    }
    Status::unimplemented(
        "this listener serves opentelemetry.proto.collector.trace.v1.TraceService/Export only",
    )
    .into_http()
}

/// The answer on the gRPC listener while ingest is closed, for a request with `headers`: `UNAVAILABLE` with
/// `message` as the reason and a `RetryInfo` of `retry_after_s` seconds, or 415 for a request that is not
/// gRPC.
pub fn grpc_unavailable(headers: &HeaderMap, retry_after_s: u32, message: &str) -> Response {
    if !is_grpc(headers) {
        return not_grpc();
    }
    reply::grpc_status(Code::Unavailable, message.to_owned(), Some(retry_after_s)).into_http()
}

impl Inner {
    /// Answers an export with an `ExportTraceServiceResponse` once its frame is durable, or with the status of
    /// its refusal.
    async fn grpc_export(
        self: &Arc<Self>,
        principal: KeyPrincipal,
        origin: Origin,
        meter: &Mutex<Meter>,
        stream: Streaming<Vec<u8>>,
    ) -> Result<tonic::Response<Vec<u8>>, Status> {
        match self.receive(principal, origin, meter, stream).await {
            Ok(acked) => {
                self.metrics.acknowledged(&acked);
                let response = reply::export_response(acked.partial.as_ref());
                Ok(tonic::Response::new(response))
            }
            Err(reject) => {
                self.metrics.refused(&reject);
                Err(reject.grpc_status())
            }
        }
    }

    /// Reads the request's message, then prepares and appends its frame as [`Inner::commit`] does for an HTTP
    /// body.
    async fn receive(
        self: &Arc<Self>,
        principal: KeyPrincipal,
        origin: Origin,
        meter: &Mutex<Meter>,
        mut stream: Streaming<Vec<u8>>,
    ) -> Result<Acked, Reject> {
        let message = stream.message().await;
        // Dropping the stream frees tonic's buffers, which the reservation covered while they filled.
        drop(stream);
        let (held, read, refused) = {
            let mut meter = meter.lock().unwrap_or_else(PoisonError::into_inner);
            (meter.held.take(), meter.read, meter.refused.take())
        };
        if let Some(reject) = refused {
            return Err(reject);
        }
        let message = match message {
            Ok(Some(message)) => message,
            Ok(None) => return Err(Reject::Malformed("the request has no message".to_owned())),
            Err(status) => return Err(decode_reject(&status)),
        };
        let mut held = held.ok_or(Reject::Internal("the request's reservation is gone"))?;
        held.set(message.len() as u64)?;
        self.charge(principal.scope.project_id(), read)?;
        let job = Job {
            wire: message,
            gzip: false,
            format: Format::Protobuf,
            transport: IngestTransport::Grpc,
            principal,
            origin,
            min_start: self.min_start(principal.scope.project_id()),
        };
        self.commit(job, held).await
    }
}
