//! A minimal OTLP/HTTP server for calibrating the generator without a real backend.
//!
//! Every request body must decode as an `ExportTraceServiceRequest` (after gunzip when the request says
//! `Content-Encoding: gzip`); one that does not is answered 400, so a run against the stub also proves the
//! generator's bodies are valid. Valid requests wait the configured delay and get the configured status with
//! an empty `ExportTraceServiceResponse` body, as `application/x-protobuf`. Paths are not checked.

use std::convert::Infallible;
use std::io::Read as _;
use std::time::Duration;

use bytes::Bytes;
use crucible_env::{Env, Listener, NetError};
use flate2::read::GzDecoder;
use http::header::{CONTENT_ENCODING, CONTENT_TYPE};
use http::{HeaderValue, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use prost::Message;

/// How the stub answers valid requests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StubConfig {
    /// The status of every answer to a valid request.
    pub status: StatusCode,
    /// How long to wait before answering.
    pub delay: Duration,
}

impl Default for StubConfig {
    /// 200 at once.
    fn default() -> Self {
        Self {
            status: StatusCode::OK,
            delay: Duration::ZERO,
        }
    }
}

/// Accepts connections on `listener` and serves each as HTTP/1.1 in its own task until the client closes
/// it. Returns only when accepting fails; abort the task to stop the stub.
pub async fn serve(
    env: Env,
    mut listener: Box<dyn Listener>,
    config: StubConfig,
) -> Result<(), NetError> {
    loop {
        let (stream, _peer) = listener.accept().await?;
        let handler_env = env.clone();
        let service = service_fn(move |request| respond(handler_env.clone(), config, request));
        // Dropping the handle detaches the connection task.
        drop(env.spawn("stub-connection", async move {
            let _closed = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        }));
    }
}

async fn respond(
    env: Env,
    config: StubConfig,
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let gzip = request
        .headers()
        .get(CONTENT_ENCODING)
        .is_some_and(|encoding| encoding.as_bytes() == b"gzip");
    let valid = match request.into_body().collect().await {
        Ok(body) => decodes(&body.to_bytes(), gzip),
        Err(_) => false,
    };
    if !valid {
        return Ok(reply(StatusCode::BAD_REQUEST, Bytes::new()));
    }
    if !config.delay.is_zero() {
        env.clock.sleep(config.delay).await;
    }
    let body = ExportTraceServiceResponse::default().encode_to_vec();
    Ok(reply(config.status, Bytes::from(body)))
}

fn decodes(body: &[u8], gzip: bool) -> bool {
    if !gzip {
        return ExportTraceServiceRequest::decode(body).is_ok();
    }
    let mut inflated = Vec::new();
    GzDecoder::new(body).read_to_end(&mut inflated).is_ok()
        && ExportTraceServiceRequest::decode(inflated.as_slice()).is_ok()
}

fn reply(status: StatusCode, body: Bytes) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-protobuf"),
    );
    response
}
