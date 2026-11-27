//! The first failed export warns once through `tracing`; later failures are only counted. The test runs alone in
//! its binary because it installs the process-wide tracing subscriber.

use std::fmt::Debug;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crucible_sdk::{Context, ObservationType, Options, observe};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

/// Keeps the message of every warning the SDK logs.
#[derive(Clone, Default)]
struct Warnings(Arc<Mutex<Vec<String>>>);

struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl Subscriber for Warnings {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let metadata = event.metadata();
        if *metadata.level() == Level::WARN && metadata.target().starts_with("crucible_sdk") {
            let mut message = Message(String::new());
            event.record(&mut message);
            self.0.lock().unwrap().push(message.0);
        }
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

#[tokio::test]
async fn the_first_failed_export_warns_once_and_later_ones_are_counted() {
    let warnings = Warnings::default();
    tracing::subscriber::set_global_default(warnings.clone()).unwrap();
    // Nothing listens on a port that was just released.
    let endpoint = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    crucible_sdk::init(Options {
        endpoint: endpoint.clone(),
        secret_key: "csk_test0000000000000000000000000000000000000".to_owned(),
        export_timeout: Duration::from_secs(2),
        flush_interval: Duration::from_secs(60),
        ..Options::default()
    })
    .unwrap();
    for _ in 0..3 {
        observe(&Context::new(), "lost", ObservationType::Span).end();
        assert!(crucible_sdk::flush().await.is_err());
    }
    assert_eq!(crucible_sdk::stats().failed, 3);
    let _stopped = crucible_sdk::shutdown().await;
    let logged = warnings.0.lock().unwrap().clone();
    assert_eq!(logged.len(), 1, "{logged:?}");
    let warning = &logged[0];
    assert!(
        warning.starts_with(&format!(
            "crucible: could not send spans to {endpoint}/v1/traces: "
        )),
        "{warning}"
    );
    assert!(
        warning.ends_with(
            "; spans go to Options::endpoint, an OTLP listener, with Options::secret_key, a secret key that has \
             the ingest scope. stats().failed counts the spans not sent; this warning is not repeated."
        ),
        "{warning}"
    );
}
