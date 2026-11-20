//! Feedback: each call posts one score to the project's feedback route with the secret key, in call order and
//! after the spans that ended before it. The call never blocks or panics; a transient failure is retried with the
//! same Idempotency-Key and body, and every score that is not sent is counted.

mod common;

use std::time::{Duration, Instant};

use common::{Collector, Export, FeedbackAnswer, GLOBAL, Mode, SECRET_KEY, init};
use crucible_sdk::{Context, Feedback, InitError, ObservationType, Options, observe};
use serde_json::{Value, json};

const PROJECT: &str = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b";
const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const ROUTE: &str = "/api/v1/projects/0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b/feedback";

async fn start(collector: &Collector) {
    init(collector, |o| o.project_id = PROJECT.to_owned()).await;
}

fn body(export: &Export) -> Value {
    serde_json::from_slice(&export.body).unwrap()
}

fn header<'a>(export: &'a Export, name: &str) -> &'a str {
    export
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// Flushes, expects an error that says every one of `parts`, and expects the next flush to be clean.
async fn flush_fails_saying(parts: &[&str]) {
    let error = crucible_sdk::flush().await.unwrap_err().to_string();
    for part in parts {
        assert!(error.contains(part), "{error:?} does not say {part:?}");
    }
    crucible_sdk::flush().await.unwrap();
}

async fn until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "{what} did not happen in time");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn helpful() -> Feedback {
    Feedback {
        trace_id: TRACE.to_owned(),
        name: "helpful".to_owned(),
        value: 1.into(),
        ..Feedback::default()
    }
}

#[tokio::test]
async fn feedback_posts_one_score_to_the_project_route_with_the_secret_key() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Accept).await;
    start(&collector).await;
    crucible_sdk::feedback(Feedback {
        comment: "good answer".to_owned(),
        idempotency_key: "thumbs-4bf9".to_owned(),
        ..helpful()
    });
    crucible_sdk::flush().await.unwrap();
    let posts = collector.feedback();
    assert_eq!(posts.len(), 1);
    let post = &posts[0];
    assert_eq!(post.path, ROUTE);
    assert_eq!(
        header(post, "authorization"),
        format!("Bearer {SECRET_KEY}")
    );
    assert_eq!(header(post, "content-type"), "application/json");
    assert_eq!(header(post, "idempotency-key"), "thumbs-4bf9");
    assert_eq!(
        body(post),
        json!({ "trace_id": TRACE, "name": "helpful", "value": 1.0, "comment": "good answer" })
    );
    let stats = crucible_sdk::stats();
    assert_eq!(
        (
            stats.feedback_sent,
            stats.feedback_dropped,
            stats.feedback_failed
        ),
        (1, 0, 0)
    );
}

#[tokio::test]
async fn every_field_goes_out_under_the_route_name_and_each_call_gets_its_own_key() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Accept).await;
    start(&collector).await;
    let metadata = json!({ "source": "thumbs", "rating": 4 });
    crucible_sdk::feedback(Feedback {
        span_id: "00f067aa0ba902b7".to_owned(),
        name: "tone".to_owned(),
        value: "friendly".into(),
        score_id: "reviewer-2".to_owned(),
        data_type: "categorical".to_owned(),
        metadata: metadata.as_object().cloned(),
        retracted: true,
        ..helpful()
    });
    crucible_sdk::feedback(Feedback {
        session_id: "conversation-17".to_owned(),
        name: "resolved".to_owned(),
        value: true.into(),
        ..Feedback::default()
    });
    crucible_sdk::flush().await.unwrap();
    let posts = collector.feedback();
    assert_eq!(posts.len(), 2);
    assert_eq!(
        body(&posts[0]),
        json!({
            "trace_id": TRACE, "span_id": "00f067aa0ba902b7", "name": "tone", "value": "friendly",
            "score_id": "reviewer-2", "data_type": "categorical", "metadata": metadata, "retracted": true,
        })
    );
    assert_eq!(
        body(&posts[1]),
        json!({ "session_id": "conversation-17", "name": "resolved", "value": true })
    );
    let keys: Vec<&str> = posts.iter().map(|p| header(p, "idempotency-key")).collect();
    for key in &keys {
        assert!(
            key.len() == 32
                && key
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{key:?} is not 32 hex characters"
        );
    }
    assert_ne!(keys[0], keys[1]);
}

#[tokio::test]
async fn spans_that_ended_before_the_call_reach_crucible_first() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Accept).await;
    start(&collector).await;
    observe(&Context::new(), "answer", ObservationType::Agent).end();
    crucible_sdk::feedback(helpful());
    until("a feedback post", || collector.feedback().len() == 1).await;
    let paths: Vec<String> = collector.exports().into_iter().map(|e| e.path).collect();
    assert_eq!(paths.first().map(String::as_str), Some("/v1/traces"));
    assert_eq!(paths.last().map(String::as_str), Some(ROUTE));
}

#[tokio::test]
async fn a_503_with_retry_after_is_retried_with_the_same_key_and_body() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Accept).await;
    start(&collector).await;
    collector.answer_feedback([FeedbackAnswer {
        status: 503,
        headers: vec![("retry-after", "0")],
        ..FeedbackAnswer::default()
    }]);
    crucible_sdk::feedback(Feedback {
        value: 0.5.into(),
        ..helpful()
    });
    crucible_sdk::flush().await.unwrap();
    let posts = collector.feedback();
    assert_eq!(posts.len(), 2);
    assert_eq!(
        header(&posts[0], "idempotency-key"),
        header(&posts[1], "idempotency-key")
    );
    assert_eq!(posts[0].body, posts[1].body);
    let stats = crucible_sdk::stats();
    assert_eq!((stats.feedback_sent, stats.feedback_failed), (1, 0));
}

#[tokio::test]
async fn a_409_conflict_is_retried_and_a_reused_key_is_not() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Accept).await;
    start(&collector).await;
    collector.answer_feedback([
        FeedbackAnswer {
            status: 409,
            headers: vec![("retry-after", "0")],
            body: r#"{"code":"conflict","status":409}"#,
        },
        FeedbackAnswer {
            status: 201,
            ..FeedbackAnswer::default()
        },
        FeedbackAnswer {
            status: 409,
            body: r#"{"code":"idempotency_key_reused","status":409}"#,
            ..FeedbackAnswer::default()
        },
    ]);
    for name in ["first", "second"] {
        crucible_sdk::feedback(Feedback {
            name: name.to_owned(),
            ..helpful()
        });
    }
    flush_fails_saying(&["answered 409 (idempotency_key_reused)"]).await;
    let names: Vec<Value> = collector
        .feedback()
        .iter()
        .map(|p| body(p)["name"].clone())
        .collect();
    assert_eq!(names, [json!("first"), json!("first"), json!("second")]);
    let stats = crucible_sdk::stats();
    assert_eq!((stats.feedback_sent, stats.feedback_failed), (1, 1));
}

#[tokio::test]
async fn a_redirect_is_not_followed_and_counts_as_failed() {
    // Following it would send the secret key to another URL and turn the post into a GET.
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Accept).await;
    start(&collector).await;
    collector.answer_feedback([FeedbackAnswer {
        status: 302,
        headers: vec![("location", "/elsewhere")],
        ..FeedbackAnswer::default()
    }]);
    crucible_sdk::feedback(helpful());
    flush_fails_saying(&["answered 302"]).await;
    let paths: Vec<String> = collector
        .exports()
        .into_iter()
        .map(|e| e.path)
        .filter(|p| p != "/v1/traces")
        .collect();
    assert_eq!(paths, [ROUTE]);
    let stats = crucible_sdk::stats();
    assert_eq!((stats.feedback_sent, stats.feedback_failed), (0, 1));
}

#[tokio::test]
async fn a_refusal_is_not_retried_and_counts_as_failed() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Accept).await;
    start(&collector).await;
    collector.answer_feedback([FeedbackAnswer {
        status: 422,
        body: r#"{"code":"validation_failed"}"#,
        ..FeedbackAnswer::default()
    }]);
    crucible_sdk::feedback(Feedback {
        name: "bad#name".to_owned(),
        ..helpful()
    });
    flush_fails_saying(&[
        &format!("{ROUTE} answered 422 (validation_failed)"),
        "Options::api_url",
        "Options::project_id",
    ])
    .await;
    assert_eq!(collector.feedback().len(), 1);
    let stats = crucible_sdk::stats();
    assert_eq!((stats.feedback_sent, stats.feedback_failed), (0, 1));
}

#[tokio::test]
async fn a_failing_connection_is_tried_three_times_then_counted() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Accept).await;
    start(&collector).await;
    collector.answer_feedback(vec![FeedbackAnswer::default(); 3]);
    crucible_sdk::feedback(helpful());
    until("a failed score", || {
        crucible_sdk::stats().feedback_failed == 1
    })
    .await;
    assert_eq!(collector.feedback().len(), 3);
    assert_eq!(crucible_sdk::stats().feedback_sent, 0);
}

#[tokio::test]
async fn feedback_returns_at_once_while_the_api_stalls() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Stall).await;
    init(&collector, |o| {
        o.project_id = PROJECT.to_owned();
        o.export_timeout = Duration::from_millis(300);
    })
    .await;
    let began = Instant::now();
    for i in 0..100 {
        crucible_sdk::feedback(Feedback {
            value: i.into(),
            ..helpful()
        });
    }
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "{:?}",
        began.elapsed()
    );
    let began = Instant::now();
    let _stopped = crucible_sdk::shutdown().await;
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "{:?}",
        began.elapsed()
    );
}

#[tokio::test]
async fn a_full_queue_drops_the_score_and_counts_it() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Stall).await;
    init(&collector, |o| {
        o.project_id = PROJECT.to_owned();
        o.export_timeout = Duration::from_millis(300);
    })
    .await;
    for i in 0..1100 {
        crucible_sdk::feedback(Feedback {
            value: i.into(),
            ..helpful()
        });
    }
    assert_eq!(crucible_sdk::stats().feedback_dropped, 76);
    let _stopped = crucible_sdk::shutdown().await;
}

#[tokio::test]
async fn init_refuses_bad_feedback_settings_with_the_option_named() {
    let _turn = GLOBAL.lock().await;
    let _stopped = crucible_sdk::shutdown().await;
    let base = || Options {
        endpoint: "http://localhost:4318".to_owned(),
        secret_key: SECRET_KEY.to_owned(),
        ..Options::default()
    };
    let error = crucible_sdk::init(Options {
        project_id: "default".to_owned(),
        ..base()
    })
    .unwrap_err();
    assert!(matches!(error, InitError::InvalidProjectId(_)), "{error:?}");
    assert!(
        error
            .to_string()
            .contains("Options::project_id \"default\" is not a project id")
    );
    let error = crucible_sdk::init(Options {
        api_url: "ftp://crucible".to_owned(),
        ..base()
    })
    .unwrap_err();
    assert!(matches!(error, InitError::InvalidApiUrl(_)), "{error:?}");
    assert!(
        error
            .to_string()
            .contains("Options::api_url \"ftp://crucible\" is not an http or https URL")
    );
}

#[tokio::test]
async fn without_a_project_id_feedback_is_dropped_and_counted() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Accept).await;
    init(&collector, |_| {}).await;
    crucible_sdk::feedback(helpful());
    flush_fails_saying(&["Options::project_id is empty"]).await;
    assert!(collector.feedback().is_empty());
    assert_eq!(crucible_sdk::stats().feedback_dropped, 1);
}

#[tokio::test]
async fn api_url_overrides_the_endpoint() {
    let _turn = GLOBAL.lock().await;
    let api = Collector::start(Mode::Accept).await;
    let _stopped = crucible_sdk::shutdown().await;
    crucible_sdk::init(Options {
        endpoint: "http://127.0.0.1:9".to_owned(),
        api_url: format!("{}/", api.endpoint),
        secret_key: SECRET_KEY.to_owned(),
        project_id: PROJECT.to_uppercase(),
        export_timeout: Duration::from_secs(1),
        ..Options::default()
    })
    .unwrap();
    crucible_sdk::feedback(helpful());
    until("a feedback post", || api.feedback().len() == 1).await;
    assert_eq!(api.feedback()[0].path, ROUTE);
    let _stopped = crucible_sdk::shutdown().await;
}

#[tokio::test]
async fn shutdown_sends_queued_feedback_and_later_feedback_is_not_sent() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Accept).await;
    start(&collector).await;
    crucible_sdk::feedback(Feedback {
        name: "last-words".to_owned(),
        ..helpful()
    });
    crucible_sdk::shutdown().await.unwrap();
    crucible_sdk::feedback(Feedback {
        name: "too-late".to_owned(),
        ..helpful()
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let names: Vec<Value> = collector
        .feedback()
        .iter()
        .map(|p| body(p)["name"].clone())
        .collect();
    assert_eq!(names, [json!("last-words")]);
}
