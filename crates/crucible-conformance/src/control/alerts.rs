//! Channels, alert rules, their state and history, the queue of evaluations and the pruning of done queue rows
//! (contracts 11.1 and 11.7).

use std::time::Duration;

use anyhow::{Context as _, Result};
use crucible_control::{
    AlertEvent, AlertEventKind, AlertRule, AlertSeriesState, AlertStatus, Channel, ChannelKind,
    JsonDoc, NewJob, Queue,
};
use crucible_records::{AlertRuleId, ChannelId, ProjectId};

use super::{Ctl, Transcript, at, id, project};

fn doc(text: &str) -> Result<JsonDoc> {
    JsonDoc::parse(text).context("the fixture document")
}

fn channel(n: u64, project_id: ProjectId, kind: ChannelKind) -> Result<Channel> {
    Ok(Channel {
        id: ChannelId::from_uuid(id(n)),
        project_id,
        name: format!("Channel {n}"),
        kind,
        config: doc(r#"{"url":"http://10.0.0.8:9000/hook"}"#)?,
        secret_id: None,
        enabled: true,
        disabled_reason: None,
        failures: 0,
        last_test: None,
        version: 1,
    })
}

fn rule(n: u64, project_id: ProjectId, enabled: bool) -> Result<AlertRule> {
    Ok(AlertRule {
        id: AlertRuleId::from_uuid(id(n)),
        project_id,
        name: format!("Rule {n}"),
        enabled,
        spec: doc(r#"{"measure":"error_count","threshold":5}"#)?,
        interval_secs: 60,
        version: 1,
        created_at: at(0),
        updated_at: at(0),
        last_eval_seq: None,
        eval_failures: 0,
        skipped_ticks: 0,
    })
}

fn state(rule_id: AlertRuleId, series: &str, status: AlertStatus, secs: i64) -> AlertSeriesState {
    AlertSeriesState {
        rule_id,
        series_key: series.to_owned(),
        status,
        since: at(secs),
        last_rows_at: at(secs),
        last_notified_at: (status == AlertStatus::Firing).then(|| at(secs)),
        value: Some(9),
    }
}

fn event(n: u64, rule_id: AlertRuleId, secs: i64) -> AlertEvent {
    AlertEvent {
        id: id(n),
        rule_id,
        series_key: r#"["prod"]"#.to_owned(),
        kind: AlertEventKind::Transition,
        from_status: AlertStatus::Ok,
        to_status: AlertStatus::Firing,
        value: Some(9),
        notified: true,
        eval_seq: n,
        at: at(secs),
    }
}

async fn channels(ctl: &Ctl, t: &mut Transcript, one: ProjectId, two: ProjectId) -> Result<()> {
    let s = ctl.store.as_ref();
    let repo = s.channels();
    let hook = channel(100, one, ChannelKind::Webhook)?;
    let mut mail = channel(101, one, ChannelKind::Email)?;
    mail.config = doc(r#"{"to":["team@example.com"]}"#)?;
    t.outcome("insert a channel", repo.insert(&hook).await);
    t.outcome("insert a second channel", repo.insert(&mail).await);
    t.outcome(
        "insert a channel of the other project",
        repo.insert(&channel(102, two, ChannelKind::Slack)?).await,
    );
    t.outcome("insert the same id", repo.insert(&hook).await);
    t.outcome(
        "insert for an unknown project",
        repo.insert(&channel(
            103,
            ProjectId::from_uuid(id(999)),
            ChannelKind::Slack,
        )?)
        .await,
    );
    t.outcome("get", repo.get(hook.id).await);
    t.outcome(
        "get an unknown channel",
        repo.get(ChannelId::from_uuid(id(199))).await,
    );
    t.outcome("list a project", repo.list(one).await);

    let mut renamed = hook.clone();
    renamed.name = "On call".to_owned();
    renamed.config = doc(r#"{"url":"https://hooks.example.com/x"}"#)?;
    renamed.secret_id = None;
    t.outcome(
        "update under a stale version",
        repo.update_if_version(&renamed, 7).await,
    );
    t.outcome("update", repo.update_if_version(&renamed, 1).await);
    t.outcome("get after the update", repo.get(hook.id).await);

    let outcome = doc(r#"{"ok":false,"status_class":"5xx"}"#)?;
    t.outcome(
        "store a test outcome",
        repo.set_last_test(hook.id, &outcome).await,
    );
    t.outcome(
        "store a test outcome of an unknown channel",
        repo.set_last_test(ChannelId::from_uuid(id(199)), &outcome)
            .await,
    );

    // Three failures turn the channel off at a limit of three, in the statement that counts the third.
    for attempt in 1..=4 {
        t.outcome(
            &format!("failure {attempt}"),
            repo.record_failure(hook.id, "three deliveries failed", 3)
                .await,
        );
    }
    t.outcome("get after the failures", repo.get(hook.id).await);
    t.outcome(
        "a success on a channel that is off",
        repo.record_success(hook.id).await,
    );
    t.outcome(
        "a failure of the second channel",
        repo.record_failure(mail.id, "x", 3).await,
    );
    t.outcome(
        "a success clears the count",
        repo.record_success(mail.id).await,
    );
    t.outcome(
        "a success with nothing to clear",
        repo.record_success(mail.id).await,
    );
    t.outcome("get the second channel", repo.get(mail.id).await);

    t.outcome(
        "delete under a stale version",
        repo.delete_if_version(mail.id, 9).await,
    );
    t.outcome("delete", repo.delete_if_version(mail.id, 1).await);
    t.outcome("delete again", repo.delete_if_version(mail.id, 1).await);
    t.outcome("list after the delete", repo.list(one).await);
    Ok(())
}

async fn rules(ctl: &Ctl, t: &mut Transcript, one: ProjectId, two: ProjectId) -> Result<()> {
    let s = ctl.store.as_ref();
    let repo = s.alert_rules();
    let first = rule(200, one, true)?;
    let off = rule(201, one, false)?;
    let other = rule(202, two, true)?;
    for new in [&first, &off, &other] {
        t.outcome("insert a rule", repo.insert(new).await);
    }
    t.outcome("insert the same id", repo.insert(&first).await);
    t.outcome(
        "insert for an unknown project",
        repo.insert(&rule(203, ProjectId::from_uuid(id(999)), true)?)
            .await,
    );
    t.outcome("get", repo.get(first.id).await);
    t.outcome("list a project", repo.list(one).await);
    t.outcome(
        "list the enabled rules",
        repo.list_enabled()
            .await
            .map(|rules| rules.iter().map(|r| r.name.clone()).collect::<Vec<_>>()),
    );

    // An evaluation writes its three columns and no version.
    t.outcome(
        "record an evaluation",
        repo.record_evaluation(first.id, Some(41), 2, 7).await,
    );
    t.outcome(
        "record an evaluation of an unknown rule",
        repo.record_evaluation(AlertRuleId::from_uuid(id(299)), Some(1), 0, 0)
            .await,
    );
    // An edit writes the definition and the version, and leaves the evaluation columns.
    let mut edited = first.clone();
    edited.name = "Edited".to_owned();
    edited.enabled = false;
    edited.interval_secs = 120;
    edited.spec = doc(r#"{"measure":"count","threshold":9}"#)?;
    edited.updated_at = at(60);
    edited.last_eval_seq = Some(999);
    t.outcome(
        "update under a stale version",
        repo.update_if_version(&edited, 4).await,
    );
    t.outcome("update", repo.update_if_version(&edited, 1).await);
    t.outcome("get after the update", repo.get(first.id).await);
    t.outcome(
        "clear the last tick",
        repo.record_evaluation(first.id, None, 0, 7).await,
    );
    t.outcome("get after the clear", repo.get(first.id).await);

    // The state of each series: created, replaced, listed in key order, removed.
    let states = s.alert_state();
    for put in [
        state(first.id, r#"["prod"]"#, AlertStatus::Pending, 10),
        state(first.id, r#"["dev"]"#, AlertStatus::Ok, 10),
        state(first.id, r#"["prod"]"#, AlertStatus::Firing, 70),
        state(other.id, "[]", AlertStatus::NoData, 10),
    ] {
        t.outcome("put a state", states.put(&put).await);
    }
    t.outcome(
        "put a state of an unknown rule",
        states
            .put(&state(
                AlertRuleId::from_uuid(id(299)),
                "[]",
                AlertStatus::Ok,
                0,
            ))
            .await,
    );
    t.outcome("list the states", states.list(first.id).await);
    t.outcome(
        "delete a state",
        states.delete(first.id, r#"["dev"]"#).await,
    );
    t.outcome(
        "delete it again",
        states.delete(first.id, r#"["dev"]"#).await,
    );

    // The history: newest first by time then id, paged by the last row of the page before.
    let events = s.alert_events();
    for (n, secs) in [(300, 60), (301, 120), (302, 120), (303, 180)] {
        t.outcome(
            "insert an event",
            events.insert(&event(n, first.id, secs)).await,
        );
    }
    t.outcome(
        "insert the same event",
        events.insert(&event(300, first.id, 60)).await,
    );
    t.outcome(
        "insert an event of an unknown rule",
        events
            .insert(&event(399, AlertRuleId::from_uuid(id(299)), 60))
            .await,
    );
    let order = |rows: Vec<AlertEvent>| rows.iter().map(|row| row.eval_seq).collect::<Vec<_>>();
    let page = events.list(first.id, None, 2).await?;
    t.note("the first page", order(page.clone()));
    let last = page.last().context("a page of two rows")?;
    t.outcome(
        "the second page",
        events
            .list(first.id, Some((last.at, last.id)), 10)
            .await
            .map(order),
    );
    t.outcome("one event in full", events.list(first.id, None, 1).await);
    t.outcome(
        "the history of another rule",
        events.list(other.id, None, 10).await.map(order),
    );

    // Deleting a rule takes its state and its history with it, and nothing of another rule.
    t.outcome(
        "delete under a stale version",
        repo.delete_if_version(first.id, 1).await,
    );
    t.outcome("delete", repo.delete_if_version(first.id, 2).await);
    t.outcome("delete again", repo.delete_if_version(first.id, 2).await);
    t.outcome("the states are gone", states.list(first.id).await);
    t.outcome(
        "the history is gone",
        events.list(first.id, None, 10).await.map(order),
    );
    t.outcome(
        "the other rule keeps its state",
        states.list(other.id).await.map(|rows| rows.len()),
    );
    Ok(())
}

/// The queue of evaluations is a queue like the outbox, and `prune_done` removes the done rows of one queue
/// that have been due for longer than the horizon.
async fn queues(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let s = ctl.store.as_ref();
    let claims = s.claims();
    let now = s.db_now().await?;
    let lease = Duration::from_secs(30);
    let hour = Duration::from_secs(3_600);
    let job = |n: u64, key: &str, due_in_secs: i64| NewJob {
        id: id(n),
        dedup_key: key.to_owned(),
        payload: format!("tick {n}"),
        run_at: now.saturating_add_micros(due_in_secs * 1_000_000),
    };
    let evaluations = Queue::AlertEvaluations;
    // Two hours overdue, a minute overdue, and not due yet.
    for new in [
        job(400, "r:60:1", -7_200),
        job(401, "r:60:2", -60),
        job(402, "r:60:3", 3_600),
    ] {
        t.outcome("enqueue a tick", claims.enqueue(evaluations, &new).await);
    }
    t.outcome(
        "enqueue the same tick again",
        claims.enqueue(evaluations, &job(403, "r:60:1", -1)).await,
    );
    // The same key in the other queue is another row.
    t.outcome(
        "enqueue the key in the outbox",
        claims
            .enqueue(Queue::NotificationOutbox, &job(404, "r:60:1", -7_200))
            .await,
    );
    let claimed = claims.claim(evaluations, 10, lease).await?;
    t.note(
        "claim the due ticks",
        claimed
            .iter()
            .map(|c| c.payload.clone())
            .collect::<Vec<_>>(),
    );
    t.outcome(
        "nothing is pruned while nothing is done",
        claims.prune_done(evaluations, hour).await,
    );
    for claim in &claimed {
        t.outcome("complete", claims.complete(claim).await);
    }
    t.outcome(
        "prune the done ticks older than an hour",
        claims.prune_done(evaluations, hour).await,
    );
    t.outcome("prune again", claims.prune_done(evaluations, hour).await);
    t.outcome(
        "the outbox is another queue",
        claims.prune_done(Queue::NotificationOutbox, hour).await,
    );
    // A pruned row's key is free again; a done row that was kept still holds its key.
    t.outcome(
        "enqueue the pruned tick again",
        claims
            .enqueue(evaluations, &job(405, "r:60:1", 3_600))
            .await,
    );
    t.outcome(
        "enqueue the kept tick again",
        claims
            .enqueue(evaluations, &job(406, "r:60:2", 3_600))
            .await,
    );
    t.outcome(
        "prune everything that is done",
        claims.prune_done(evaluations, Duration::ZERO).await,
    );
    Ok(())
}

pub(super) async fn run(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let s = ctl.store.as_ref();
    let (one, two) = (project(1), project(2));
    s.projects().insert(&one).await?;
    s.projects().insert(&two).await?;
    channels(ctl, t, one.id, two.id).await?;
    rules(ctl, t, one.id, two.id).await?;
    queues(ctl, t).await
}
