//! What each kind of channel is sent (contract 12.6), and the signature of a webhook.

use crucible_records::{Timestamp, Uuid};
use hmac::{Hmac, KeyInit as _, Mac as _};
use serde_json::{Map, Value, json};
use sha2::Sha256;

use crate::notification::{NoticeKind, Notification};

/// The header that carries a webhook's signature.
pub const SIGNATURE_HEADER: &str = "x-crucible-signature";
/// The header that carries the delivery id, the same at every retry: receivers deduplicate on it.
pub const DELIVERY_HEADER: &str = "x-crucible-delivery";
/// The header that carries the event name, `alert.<kind>`.
pub const EVENT_HEADER: &str = "x-crucible-event";

fn comparison_words(comparison: &str) -> &'static str {
    match comparison {
        "gt" => "above",
        "gte" => "at or above",
        "lt" => "below",
        "lte" => "at or below",
        _ => "compared with",
    }
}

fn window_words(secs: u32) -> String {
    match secs {
        s if s % 3_600 == 0 => format!("{} h", s / 3_600),
        s => format!("{} min", s / 60),
    }
}

/// One sentence that says what happened.
pub fn summary(n: &Notification) -> String {
    let what = format!("{} ({})", n.measure, n.aggregation);
    let window = window_words(n.window_secs);
    let condition = format!("{} {}", comparison_words(&n.comparison), n.threshold);
    match (n.kind, &n.value) {
        (NoticeKind::Test, _) => {
            "This is a test notification from Crucible. The channel works.".to_owned()
        }
        (NoticeKind::EvaluationFailing, _) => {
            "The rule could not be evaluated five times in a row. Its state is unchanged until an evaluation \
             succeeds."
                .to_owned()
        }
        (NoticeKind::NoData, _) => format!("{what} has had no data over the last {window}."),
        (NoticeKind::Resolved, Some(value)) => {
            format!("{what} is {value}, no longer {condition}, over the last {window}.")
        }
        (NoticeKind::Resolved, None) => format!("{what} is no longer {condition}."),
        (_, Some(value)) => format!("{what} is {value}, {condition}, over the last {window}."),
        (_, None) => format!("{what} is {condition}."),
    }
}

fn title(n: &Notification) -> String {
    format!("{}: {}", n.kind.headline(), n.rule_name)
}

/// The link to the rule in the UI, when the deployment knows its public URL.
fn link(n: &Notification, public_url: Option<&str>) -> Option<String> {
    public_url.map(|base| format!("{}/alerts/?rule={}", base.trim_end_matches('/'), n.rule_id))
}

/// The body of a Slack incoming-webhook call.
pub fn slack_body(n: &Notification, public_url: Option<&str>) -> String {
    let mut lines = vec![format!("*{}*", title(n)), summary(n)];
    let series = n.series_text();
    if !series.is_empty() {
        lines.push(series);
    }
    lines.extend(link(n, public_url));
    json!({ "text": lines.join("\n") }).to_string()
}

/// The subject and the text of an email.
pub fn email(n: &Notification, public_url: Option<&str>) -> (String, String) {
    let mut lines = vec![summary(n)];
    let series = n.series_text();
    if !series.is_empty() {
        lines.push(format!("Series: {series}"));
    }
    lines.push(format!("Rule: {}", n.rule_name));
    lines.push(format!("Evaluated at: {}", n.at()));
    if let Some(link) = link(n, public_url) {
        lines.push(format!("Open the rule: {link}"));
    }
    (format!("[Crucible] {}", title(n)), lines.join("\n"))
}

/// The JSON body of a webhook call.
pub fn webhook_body(
    n: &Notification,
    delivery_id: Uuid,
    sent_at: Timestamp,
    public_url: Option<&str>,
) -> String {
    let series: Map<String, Value> = n
        .series
        .iter()
        .map(|label| (label.dimension.clone(), Value::String(label.value.clone())))
        .collect();
    json!({
        "event": format!("alert.{}", n.kind.as_str()),
        "delivery_id": delivery_id.to_string(),
        "sent_at": sent_at.to_string(),
        "project_id": n.project_id.to_string(),
        "rule": { "id": n.rule_id.to_string(), "name": n.rule_name },
        "state": n.kind.as_str(),
        "series": series,
        "measure": n.measure,
        "aggregation": n.aggregation,
        "comparison": n.comparison,
        "threshold": n.threshold,
        "value": n.value,
        "window_seconds": n.window_secs,
        "evaluated_at": n.at().to_string(),
        "summary": summary(n),
        "url": link(n, public_url),
    })
    .to_string()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The value of `X-Crucible-Signature` (contract 12.6): `t=<unix>,v1=<hex HMAC-SHA256 of "t.body">`.
pub fn signature(secret: &[u8], unix_secs: i64, body: &str) -> String {
    // HMAC takes a key of any length.
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return String::new();
    };
    mac.update(unix_secs.to_string().as_bytes());
    mac.update(b".");
    mac.update(body.as_bytes());
    format!("t={unix_secs},v1={}", hex(&mac.finalize().into_bytes()))
}

/// What a receiver does with the header: recompute the MAC over `t.body` under the shared secret and compare
/// in constant time. Returns the signed time when the signature is this body's.
pub fn verify_signature(secret: &[u8], header: &str, body: &str) -> Option<i64> {
    let mut t = None;
    let mut v1 = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", value)) => t = value.parse::<i64>().ok(),
            Some(("v1", value)) => v1.push(value),
            _ => {}
        }
    }
    let t = t?;
    v1.into_iter().find_map(|given| {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).ok()?;
        mac.update(t.to_string().as_bytes());
        mac.update(b".");
        mac.update(body.as_bytes());
        let bytes: Vec<u8> = (0..given.len())
            .step_by(2)
            .map(|i| {
                given
                    .get(i..i + 2)
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok())
            })
            .collect::<Option<_>>()?;
        mac.verify_slice(&bytes).ok().map(|()| t)
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::notification::tests::notification;

    #[test]
    fn each_kind_reads_as_a_sentence() {
        let firing = notification(NoticeKind::Firing);
        assert_eq!(
            summary(&firing),
            "error_count (sum) is 9, above 5, over the last 5 min."
        );
        assert_eq!(
            summary(&notification(NoticeKind::Resolved)),
            "error_count (sum) is 9, no longer above 5, over the last 5 min."
        );
        assert_eq!(
            summary(&notification(NoticeKind::NoData)),
            "error_count (sum) has had no data over the last 5 min."
        );
        let slack: Value =
            serde_json::from_str(&slack_body(&firing, Some("https://crucible.test/"))).unwrap();
        let text = slack["text"].as_str().unwrap();
        assert!(text.starts_with("*Firing: High error rate*\n"));
        assert!(text.contains("environment=prod"));
        assert!(text.ends_with(&format!(
            "https://crucible.test/alerts/?rule={}",
            firing.rule_id
        )));
        let (subject, body) = email(&firing, None);
        assert_eq!(subject, "[Crucible] Firing: High error rate");
        assert!(body.contains("Series: environment=prod") && !body.contains("Open the rule"));
    }

    #[test]
    fn a_webhook_body_is_json_with_the_event_and_the_delivery() {
        let n = notification(NoticeKind::Renotify);
        let delivery = Uuid::from_v7_parts(1_800_000_000_123, 9);
        let body = webhook_body(
            &n,
            delivery,
            Timestamp::from_micros(1_800_000_001_000_000),
            None,
        );
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["event"], "alert.renotify");
        assert_eq!(value["delivery_id"], delivery.to_string());
        assert_eq!(value["series"]["environment"], "prod");
        assert_eq!(value["rule"]["name"], "High error rate");
        assert_eq!(value["url"], Value::Null);
        assert_eq!(value["window_seconds"], 300);
    }

    #[test]
    fn the_signature_has_the_shape_of_the_contract() {
        let header = signature(b"key", 1_700_000_000, "{}");
        assert!(header.starts_with("t=1700000000,v1="));
        assert_eq!(header.len(), "t=1700000000,v1=".len() + 64);
        assert_eq!(verify_signature(b"key", &header, "{}"), Some(1_700_000_000));
    }

    proptest! {
        /// A reference receiver accepts exactly the body that was signed under the secret it shares.
        #[test]
        fn a_receiver_verifies_what_was_signed(
            secret in proptest::collection::vec(any::<u8>(), 1..64),
            t in 0i64..4_102_444_800,
            body in ".{0,200}",
            other in ".{1,20}",
        ) {
            let header = signature(&secret, t, &body);
            prop_assert_eq!(verify_signature(&secret, &header, &body), Some(t));
            let tampered = format!("{body}{other}");
            prop_assert_eq!(verify_signature(&secret, &header, &tampered), None);
            let mut wrong = secret.clone();
            wrong[0] ^= 1;
            prop_assert_eq!(verify_signature(&wrong, &header, &body), None);
            // During a rotation the header may carry two `v1` values; either one verifies.
            let both = format!("{header},v1={}", "0".repeat(64));
            prop_assert_eq!(verify_signature(&secret, &both, &body), Some(t));
        }
    }
}
