//! What an export reads and how it writes: the selector and the options, as the job row stores them (JSON text,
//! opaque to the control store).

use crucible_plan::MAX_LIST;
use crucible_query::ExportQuery;
use crucible_records::{TimeWindow, Timestamp, TraceId};
use serde::{Deserialize, Serialize};

/// What an export holds: the spans that start inside the window, narrowed by the optional conditions, all of
/// which must hold.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Selector {
    /// The window over `start_time`.
    pub window: TimeWindow,
    /// Only these traces; every trace when empty. At most [`MAX_TRACE_IDS`].
    pub trace_ids: Vec<TraceId>,
    /// Only spans of this user.
    pub user_id: Option<String>,
    /// Only spans of this session.
    pub session_id: Option<String>,
}

/// The most trace ids one selector names.
pub const MAX_TRACE_IDS: usize = MAX_LIST;

/// How an export is written.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Options {
    /// The file is gzip-compressed.
    #[serde(default)]
    pub gzip: bool,
    /// `otlp-json` writes spans with a field a cap cut, their cut fields as opaque text, instead of skipping
    /// them (contract 12.8; the default skips them).
    #[serde(default)]
    pub keep_truncated: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    from: String,
    to: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    trace_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

/// A stored selector or options document that does not read back.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[error("the stored export {0} does not read back")]
pub struct Unreadable(pub &'static str);

impl Selector {
    /// The selector as the job row stores it.
    pub fn to_text(&self) -> String {
        let stored = Stored {
            from: self.window.from().to_string(),
            to: self.window.to().to_string(),
            trace_ids: self.trace_ids.iter().map(ToString::to_string).collect(),
            user_id: self.user_id.clone(),
            session_id: self.session_id.clone(),
        };
        serde_json::to_string(&stored).unwrap_or_default()
    }

    /// The selector of a stored job row.
    pub fn from_text(text: &str) -> Result<Self, Unreadable> {
        let bad = || Unreadable("selector");
        let stored: Stored = serde_json::from_str(text).map_err(|_| bad())?;
        let time = |s: &str| Timestamp::parse_rfc3339(s).map_err(|_| bad());
        let window = TimeWindow::new(time(&stored.from)?, time(&stored.to)?).map_err(|_| bad())?;
        let trace_ids = stored
            .trace_ids
            .iter()
            .map(|id| TraceId::parse(id).map_err(|_| bad()))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            window,
            trace_ids,
            user_id: stored.user_id,
            session_id: stored.session_id,
        })
    }

    /// The reads of the export, under its fixed `as_of`.
    pub fn query(&self, as_of: Timestamp) -> ExportQuery {
        ExportQuery {
            window: self.window,
            trace_ids: self.trace_ids.clone(),
            user_id: self.user_id.clone(),
            session_id: self.session_id.clone(),
            as_of,
        }
    }
}

impl Options {
    /// The options as the job row stores them.
    pub fn to_text(self) -> String {
        serde_json::to_string(&self).unwrap_or_default()
    }

    /// The options of a stored job row.
    pub fn from_text(text: &str) -> Result<Self, Unreadable> {
        serde_json::from_str(text).map_err(|_| Unreadable("options"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selector_and_options_read_back() {
        let window = TimeWindow::new(Timestamp::from_micros(1), Timestamp::from_micros(2_000_000)).unwrap();
        let selector = Selector {
            window,
            trace_ids: vec![TraceId::from_bytes([0xab; 16])],
            user_id: Some("u".to_owned()),
            session_id: None,
        };
        assert_eq!(Selector::from_text(&selector.to_text()), Ok(selector));
        let options = Options {
            gzip: true,
            keep_truncated: false,
        };
        assert_eq!(Options::from_text(&options.to_text()), Ok(options));
        assert_eq!(Options::from_text("{}"), Ok(Options::default()));
        assert!(Options::from_text("{\"zip\":true}").is_err());
        assert!(Selector::from_text("{\"from\":\"x\",\"to\":\"y\"}").is_err());
    }
}
