//! The state machine of one series of a rule (contract 12.6). Pure: a function of the rule's timing, the
//! series' stored state, what the evaluation observed and the time of the tick.
//!
//! | From | Observation | To | Notice |
//! |---|---|---|---|
//! | none, ok | breach | pending, or firing when `for` is 0 | firing, when it fires |
//! | pending | breach held for `for` | firing | firing |
//! | pending | no breach | ok | |
//! | firing | no breach | resolved | resolved |
//! | firing | breach, `renotify` after the last notice | firing | renotify |
//! | resolved | no breach | ok | |
//! | resolved | breach | firing | firing |
//! | no_data | rows without a breach | ok | |
//! | no_data | breach | pending, or firing when `for` is 0 | firing, when it fires |
//! | any | no rows for less than `no_data_for` | unchanged | renotify, when firing and due |
//! | any | no rows for `no_data_for` | per policy: no_data, as "no breach", or unchanged | no_data, once |
//!
//! A series that was never seen and has no rows has no state. A series that is `ok` or `no_data` and has had
//! no rows for [`FORGET_AFTER`] is forgotten.

use std::time::Duration;

use crucible_control::AlertStatus;
use crucible_records::Timestamp;

use crate::spec::NoDataPolicy;

/// How long a series without rows is kept once it is `ok` or `no_data`.
pub const FORGET_AFTER: Duration = Duration::from_secs(86_400);

const MICROS: i64 = 1_000_000;

/// The timing of a rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Params {
    /// How long the condition must hold before the series fires, in seconds.
    pub for_secs: u32,
    /// How long after a notice a still-firing series gets another, in seconds.
    pub renotify_secs: u32,
    /// The no-data policy.
    pub no_data: NoDataPolicy,
    /// How long a series may have no rows before the policy applies, in seconds.
    pub no_data_for_secs: u32,
}

/// What an evaluation saw of one series.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Observation {
    /// The series had rows in the window.
    Value {
        /// The value.
        value: i64,
        /// Whether it breaches the threshold.
        breach: bool,
    },
    /// The series had no rows in the window.
    NoRows,
}

/// The state of one series.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Series {
    /// The state.
    pub status: AlertStatus,
    /// The tick at which the series entered it.
    pub since: Timestamp,
    /// The last tick at which the series had rows.
    pub last_rows_at: Timestamp,
    /// The tick of the last firing notice.
    pub last_notified_at: Option<Timestamp>,
    /// The value at the last tick with rows.
    pub value: Option<i64>,
}

/// What a step tells the channels.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Notice {
    /// The series started firing.
    Firing,
    /// The series is still firing.
    Renotify,
    /// The series stopped firing.
    Resolved,
    /// The series has no data.
    NoData,
}

impl Notice {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Firing => "firing",
            Self::Renotify => "renotify",
            Self::Resolved => "resolved",
            Self::NoData => "no_data",
        }
    }
}

/// A seeded defect of the feature `test-mutants`, for the simulation that must catch each. Product code never
/// builds one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mutant {
    /// The outbox rows of a transition are written after its transaction, not in it.
    NotifyOutsideTransaction,
    /// A breach fires at once, whatever the rule's `for`.
    SkipForDuration,
    /// A series without rows keeps its state, whatever the rule's no-data policy.
    IgnoreNoDataPolicy,
}

/// The outcome of one step.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Step {
    /// The state after; `None` when the series has none.
    pub next: Option<Series>,
    /// What to tell the channels, if anything.
    pub notice: Option<Notice>,
}

const fn micros(secs: u32) -> i64 {
    secs as i64 * MICROS
}

/// A firing series that keeps firing: notified again once `renotify` has passed since its last notice.
fn still_firing(params: &Params, prev: Series, at: Timestamp) -> Step {
    let last = prev.last_notified_at.unwrap_or(prev.since);
    if at.micros().saturating_sub(last.micros()) >= micros(params.renotify_secs) {
        Step {
            next: Some(Series {
                last_notified_at: Some(at),
                ..prev
            }),
            notice: Some(Notice::Renotify),
        }
    } else {
        Step {
            next: Some(prev),
            notice: None,
        }
    }
}

/// The step of a series whose condition was judged: `breach` or not. `rows` carries the value when the
/// judgement comes from rows, and is `None` when the `ok` no-data policy stands in for them.
fn judged(
    params: &Params,
    prev: Option<Series>,
    breach: bool,
    rows: Option<i64>,
    at: Timestamp,
    for_micros: i64,
) -> Step {
    let base = Series {
        status: prev.map_or(AlertStatus::Ok, |p| p.status),
        since: prev.map_or(at, |p| p.since),
        last_rows_at: match (rows, prev) {
            (None, Some(prev)) => prev.last_rows_at,
            _ => at,
        },
        last_notified_at: prev.and_then(|p| p.last_notified_at),
        value: rows.or(prev.and_then(|p| p.value)),
    };
    let enter = |status: AlertStatus, notice: Option<Notice>| Step {
        next: Some(Series {
            status,
            since: at,
            last_notified_at: match notice {
                Some(Notice::Firing) => Some(at),
                _ => base.last_notified_at,
            },
            ..base
        }),
        notice,
    };
    let stay = Step {
        next: Some(base),
        notice: None,
    };
    let fire = || enter(AlertStatus::Firing, Some(Notice::Firing));
    match (prev.map(|p| p.status), breach) {
        (None | Some(AlertStatus::Ok | AlertStatus::NoData), true) => {
            if for_micros == 0 {
                fire()
            } else {
                enter(AlertStatus::Pending, None)
            }
        }
        (None, false) => enter(AlertStatus::Ok, None),
        (Some(AlertStatus::Ok), false) => stay,
        (Some(AlertStatus::NoData | AlertStatus::Pending | AlertStatus::Resolved), false) => {
            enter(AlertStatus::Ok, None)
        }
        (Some(AlertStatus::Pending), true) => {
            if at.micros().saturating_sub(base.since.micros()) >= for_micros {
                fire()
            } else {
                stay
            }
        }
        (Some(AlertStatus::Firing), true) => still_firing(params, base, at),
        (Some(AlertStatus::Firing), false) => enter(AlertStatus::Resolved, Some(Notice::Resolved)),
        (Some(AlertStatus::Resolved), true) => fire(),
    }
}

/// One evaluation of one series at tick `at`.
pub fn step(
    params: &Params,
    prev: Option<Series>,
    observation: Observation,
    at: Timestamp,
    mutant: Option<Mutant>,
) -> Step {
    let for_micros = match mutant {
        Some(Mutant::SkipForDuration) => 0,
        _ => micros(params.for_secs),
    };
    let prev = match (observation, prev) {
        (Observation::Value { value, breach }, prev) => {
            return judged(params, prev, breach, Some(value), at, for_micros);
        }
        (Observation::NoRows, None) => {
            return Step {
                next: None,
                notice: None,
            };
        }
        (Observation::NoRows, Some(prev)) => prev,
    };
    let unchanged = |prev: Series| match prev.status {
        AlertStatus::Firing => still_firing(params, prev, at),
        _ => Step {
            next: Some(prev),
            notice: None,
        },
    };
    if mutant == Some(Mutant::IgnoreNoDataPolicy) {
        return unchanged(prev);
    }
    let silent = at.micros().saturating_sub(prev.last_rows_at.micros());
    let forget = i64::try_from(FORGET_AFTER.as_micros()).unwrap_or(i64::MAX);
    if silent >= forget && matches!(prev.status, AlertStatus::Ok | AlertStatus::NoData) {
        return Step {
            next: None,
            notice: None,
        };
    }
    if silent < micros(params.no_data_for_secs) {
        return unchanged(prev);
    }
    match params.no_data {
        NoDataPolicy::Keep => unchanged(prev),
        NoDataPolicy::Ok => judged(params, Some(prev), false, None, at, for_micros),
        NoDataPolicy::NoData if prev.status == AlertStatus::NoData => Step {
            next: Some(prev),
            notice: None,
        },
        NoDataPolicy::NoData => Step {
            next: Some(Series {
                status: AlertStatus::NoData,
                since: at,
                ..prev
            }),
            notice: Some(Notice::NoData),
        },
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const PARAMS: Params = Params {
        for_secs: 120,
        renotify_secs: 600,
        no_data: NoDataPolicy::NoData,
        no_data_for_secs: 180,
    };

    fn at(minute: i64) -> Timestamp {
        Timestamp::from_micros(1_800_000_000 * MICROS + minute * 60 * MICROS)
    }

    const BREACH: Observation = Observation::Value {
        value: 9,
        breach: true,
    };
    const CLEAN: Observation = Observation::Value {
        value: 1,
        breach: false,
    };

    /// Runs the observations at one tick per minute and returns the state and notice after each.
    fn run(params: &Params, observations: &[Observation]) -> Vec<(Option<AlertStatus>, Option<Notice>)> {
        let mut state = None;
        observations
            .iter()
            .enumerate()
            .map(|(minute, observation)| {
                let step = step(params, state, *observation, at(minute as i64), None);
                state = step.next;
                (state.map(|s| s.status), step.notice)
            })
            .collect()
    }

    use AlertStatus::{Firing, NoData, Ok as Okay, Pending, Resolved};

    #[test]
    fn a_breach_is_pending_for_the_for_duration_then_fires_once() {
        let trace = run(&PARAMS, &[CLEAN, BREACH, BREACH, BREACH, BREACH, CLEAN, CLEAN]);
        assert_eq!(
            trace,
            vec![
                (Some(Okay), None),
                (Some(Pending), None),
                (Some(Pending), None),
                (Some(Firing), Some(Notice::Firing)),
                (Some(Firing), None),
                (Some(Resolved), Some(Notice::Resolved)),
                (Some(Okay), None),
            ]
        );
    }

    #[test]
    fn a_breach_that_ends_before_the_for_duration_never_notifies() {
        let trace = run(&PARAMS, &[BREACH, BREACH, CLEAN, BREACH, CLEAN]);
        assert!(trace.iter().all(|(_, notice)| notice.is_none()));
        assert_eq!(trace[2].0, Some(Okay));
    }

    #[test]
    fn without_a_for_duration_the_first_breach_fires() {
        let now = Params {
            for_secs: 0,
            ..PARAMS
        };
        assert_eq!(
            run(&now, &[BREACH, CLEAN, BREACH]),
            vec![
                (Some(Firing), Some(Notice::Firing)),
                (Some(Resolved), Some(Notice::Resolved)),
                // A resolved series that breaches again fires at once.
                (Some(Firing), Some(Notice::Firing)),
            ]
        );
    }

    #[test]
    fn a_firing_series_is_notified_again_after_renotify() {
        let now = Params {
            for_secs: 0,
            ..PARAMS
        };
        let trace = run(&now, &[BREACH; 22]);
        let notices: Vec<usize> = trace
            .iter()
            .enumerate()
            .filter_map(|(minute, (_, notice))| notice.map(|_| minute))
            .collect();
        assert_eq!(notices, [0, 10, 20]);
        assert_eq!(trace[10].1, Some(Notice::Renotify));
    }

    #[test]
    fn rows_that_stop_follow_the_policy_after_the_no_data_time() {
        use Observation::NoRows;
        // no_data: unchanged for three minutes, then the state `no_data` with one notice.
        let trace = run(&PARAMS, &[CLEAN, NoRows, NoRows, NoRows, NoRows, CLEAN]);
        assert_eq!(
            trace,
            vec![
                (Some(Okay), None),
                (Some(Okay), None),
                (Some(Okay), None),
                (Some(NoData), Some(Notice::NoData)),
                (Some(NoData), None),
                (Some(Okay), None),
            ]
        );
        // ok: a firing series resolves when its rows stop.
        let ok = Params {
            for_secs: 0,
            no_data: NoDataPolicy::Ok,
            ..PARAMS
        };
        let trace = run(&ok, &[BREACH, NoRows, NoRows, NoRows, NoRows]);
        assert_eq!(trace[2], (Some(Firing), None));
        assert_eq!(trace[3], (Some(Resolved), Some(Notice::Resolved)));
        assert_eq!(trace[4], (Some(Okay), None));
        // keep: a firing series keeps firing, and is notified again when due.
        let keep = Params {
            for_secs: 0,
            no_data: NoDataPolicy::Keep,
            ..PARAMS
        };
        let mut observations = vec![BREACH];
        observations.extend([NoRows; 11]);
        let trace = run(&keep, &observations);
        assert!(trace.iter().all(|(status, _)| *status == Some(Firing)));
        assert_eq!(trace[10].1, Some(Notice::Renotify));
    }

    #[test]
    fn a_series_never_seen_has_no_state_and_a_silent_one_is_forgotten() {
        assert_eq!(
            step(&PARAMS, None, Observation::NoRows, at(0), None),
            Step {
                next: None,
                notice: None
            }
        );
        let seen = step(&PARAMS, None, CLEAN, at(0), None).next;
        let day_later = step(&PARAMS, seen, Observation::NoRows, at(24 * 60), None);
        assert_eq!(day_later.next, None);
        // A firing series under `keep` is never forgotten.
        let keep = Params {
            for_secs: 0,
            no_data: NoDataPolicy::Keep,
            ..PARAMS
        };
        let firing = step(&keep, None, BREACH, at(0), None).next;
        assert!(step(&keep, firing, Observation::NoRows, at(48 * 60), None).next.is_some());
    }

    #[test]
    fn the_mutants_change_what_they_name() {
        let skipped = step(&PARAMS, None, BREACH, at(0), Some(Mutant::SkipForDuration));
        assert_eq!(skipped.notice, Some(Notice::Firing));
        let seen = step(&PARAMS, None, CLEAN, at(0), None).next;
        let ignored = step(
            &PARAMS,
            seen,
            Observation::NoRows,
            at(10),
            Some(Mutant::IgnoreNoDataPolicy),
        );
        assert_eq!(ignored.next.map(|s| s.status), Some(Okay));
        assert_eq!(ignored.notice, None);
    }

    fn observation() -> impl Strategy<Value = Observation> {
        prop_oneof![
            (0i64..20).prop_map(|value| Observation::Value { value, breach: value > 10 }),
            Just(Observation::NoRows),
        ]
    }

    fn params() -> impl Strategy<Value = Params> {
        (
            prop_oneof![Just(0u32), Just(60), Just(120), Just(300)],
            prop_oneof![Just(300u32), Just(600)],
            prop_oneof![
                Just(NoDataPolicy::NoData),
                Just(NoDataPolicy::Ok),
                Just(NoDataPolicy::Keep)
            ],
            prop_oneof![Just(0u32), Just(120), Just(300)],
        )
            .prop_map(|(for_secs, renotify_secs, no_data, no_data_for_secs)| Params {
                for_secs,
                renotify_secs,
                no_data,
                no_data_for_secs,
            })
    }

    proptest! {
        /// Over any history: a notice comes only with the state it names; firing notices are at least
        /// `for` after the breach began; a resolved notice follows a firing state; two notices of a
        /// firing spell are at least `renotify` apart; and a series is `pending` only while it breaches.
        #[test]
        fn notices_follow_the_states(
            params in params(),
            observations in proptest::collection::vec(observation(), 1..120),
        ) {
            let mut state: Option<Series> = None;
            let mut breach_since: Option<i64> = None;
            let mut last_notice: Option<i64> = None;
            for (minute, observation) in observations.iter().enumerate() {
                let minute = minute as i64;
                let before = state;
                let step = step(&params, state, *observation, at(minute), None);
                state = step.next;
                let status = state.map(|s| s.status);
                match observation {
                    Observation::Value { breach: true, .. } => {
                        breach_since.get_or_insert(minute);
                    }
                    Observation::Value { breach: false, .. } => breach_since = None,
                    Observation::NoRows => {}
                }
                match step.notice {
                    Some(Notice::Firing) => {
                        prop_assert_eq!(status, Some(Firing));
                        prop_assert_ne!(before.map(|s| s.status), Some(Firing));
                        if before.map(|s| s.status) != Some(Resolved) {
                            let began = breach_since.expect("a firing notice needs a breach");
                            prop_assert!((minute - began) * 60 >= i64::from(params.for_secs));
                        }
                        last_notice = Some(minute);
                    }
                    Some(Notice::Renotify) => {
                        prop_assert_eq!(status, Some(Firing));
                        prop_assert_eq!(before.map(|s| s.status), Some(Firing));
                        let last = last_notice.expect("a repeat follows a notice");
                        prop_assert!((minute - last) * 60 >= i64::from(params.renotify_secs));
                        last_notice = Some(minute);
                    }
                    Some(Notice::Resolved) => {
                        prop_assert_eq!(status, Some(Resolved));
                        prop_assert_eq!(before.map(|s| s.status), Some(Firing));
                    }
                    Some(Notice::NoData) => {
                        prop_assert_eq!(status, Some(NoData));
                        prop_assert_ne!(before.map(|s| s.status), Some(NoData));
                        prop_assert_eq!(params.no_data, NoDataPolicy::NoData);
                        prop_assert_eq!(*observation, Observation::NoRows);
                    }
                    None => {
                        // Entering or leaving the firing state never goes without a notice, except into
                        // `no_data`, which has its own.
                        let was = before.map(|s| s.status) == Some(Firing);
                        let is = status == Some(Firing);
                        prop_assert_eq!(was, is, "{:?} -> {:?} without a notice", before, state);
                    }
                }
                if status == Some(Pending) {
                    prop_assert!(breach_since.is_some());
                }
                if let Some(state) = state {
                    prop_assert!(state.since <= at(minute) && state.last_rows_at <= at(minute));
                }
            }
        }
    }
}
