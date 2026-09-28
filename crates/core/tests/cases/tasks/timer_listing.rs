//! BE-B6: the caller's account-wide timer list (`account_timer_sessions`).
use anyhow::Result;
use atlas_core::{Command, Store, tasks::*};
use serde_json::Value;

use crate::support::task_workflows::{account, id, setup, task};
use crate::support::tasks::{at, create, definition, shared, view};

const NOW: i64 = 1788868800;

fn seconds() -> Goal {
    Goal::Numeric {
        minimum: Some(Decimal("60".into())),
        maximum: None,
        unit: "seconds".into(),
    }
}

async fn run(s: &Store, a: &str, c: TaskCommand, now: i64) -> Result<()> {
    s.task_command(a, &id(), &c, None, now).await?;
    Ok(())
}

async fn start(s: &Store, a: &str, o: &str, session: &str, from: i64) -> Result<()> {
    run(
        s,
        a,
        TaskCommand::StartTimer {
            occurrence_id: o.into(),
            session_id: session.into(),
            started_at: from,
        },
        NOW,
    )
    .await
}

async fn stop(s: &Store, a: &str, o: &str, session: &str, to: i64) -> Result<()> {
    run(
        s,
        a,
        TaskCommand::StopTimer {
            occurrence_id: o.into(),
            session_id: session.into(),
            expected_version: 1,
            stopped_at: to,
        },
        NOW,
    )
    .await
}

fn ids(page: &AccountTimerPage) -> Vec<String> {
    page.items.iter().map(session_id).collect()
}

fn session_id(item: &AccountTimerSession) -> String {
    match item {
        AccountTimerSession::Available { id, .. } | AccountTimerSession::Restricted { id, .. } => {
            id.clone()
        }
    }
}

fn code(error: anyhow::Error) -> String {
    error.to_string()
}

/// Every id the store holds for the account in the listing's own order, from an independent
/// query. Running rows are ordered by the listing itself (start time, then id, newest first, in
/// Rust); finished rows by the database (`ORDER BY stopped_at DESC,id DESC`, so the tie-break
/// follows its collation and is read, not assumed).
async fn expected_order(s: &Store, a: &str) -> Result<Vec<String>> {
    let mut running: Vec<(i64, String)> = sqlx::query_as(
        "SELECT started_at,id FROM timer_sessions WHERE account_id=$1 AND cancelled=0 AND stopped_at IS NULL",
    )
    .bind(a)
    .fetch_all(&s.pool)
    .await?;
    running.sort_by(|x, y| y.cmp(x));
    let mut order: Vec<String> = running.into_iter().map(|(_, id)| id).collect();
    order.extend(
        sqlx::query_scalar::<_, String>(
            "SELECT id FROM timer_sessions WHERE account_id=$1 AND cancelled=0 AND stopped_at IS NOT NULL ORDER BY stopped_at DESC,id DESC",
        )
        .bind(a)
        .fetch_all(&s.pool)
        .await?,
    );
    Ok(order)
}

async fn walk(s: &Store, a: &str, state: TimerState, limit: u16) -> Result<Vec<AccountTimerPage>> {
    let mut pages = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = s
            .account_timer_sessions(a, state, after.as_deref(), limit)
            .await?;
        after = page.next_after.clone();
        pages.push(page);
        if after.is_none() {
            return Ok(pages);
        }
        assert!(pages.len() < 100, "the walk terminates");
    }
}

/// Seven timers with ties in both segments and one cancelled session that must never appear.
async fn seed(s: &Store, a: &str) -> Result<Vec<String>> {
    let mut all = Vec::new();
    // Running: two share a start time (tie broken by id); one is older.
    for (from, count) in [(NOW - 400, 2), (NOW - 500, 1)] {
        for _ in 0..count {
            let (o, session) = (task(s, a, seconds()).await?, id());
            start(s, a, &o, &session, from).await?;
            all.push(session);
        }
    }
    // Stopped: two share a finish time; the others are older.
    for (from, to, count) in [
        (NOW - 900, NOW - 800, 2),
        (NOW - 1900, NOW - 1000, 1),
        (NOW - 3000, NOW - 2000, 1),
    ] {
        for _ in 0..count {
            let (o, session) = (task(s, a, seconds()).await?, id());
            start(s, a, &o, &session, from).await?;
            stop(s, a, &o, &session, to).await?;
            all.push(session);
        }
    }
    // Discarded: excluded from every listing.
    let (o, gone) = (task(s, a, seconds()).await?, id());
    start(s, a, &o, &gone, NOW - 100).await?;
    run(
        s,
        a,
        TaskCommand::CancelTimer {
            occurrence_id: o,
            session_id: gone,
            expected_version: 1,
        },
        NOW,
    )
    .await?;
    Ok(all)
}

#[tokio::test]
async fn timers_are_listed_running_first_then_by_finish_time_and_paged_by_keyset() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let b = account(&s).await?;
    let seeded = seed(&s, &a).await?;
    // Another account's timers never appear in this account's list, and vice versa.
    let (ob, other) = (task(&s, &b, seconds()).await?, id());
    start(&s, &b, &ob, &other, NOW - 50).await?;

    let all = s
        .account_timer_sessions(&a, TimerState::All, None, 200)
        .await?;
    assert_eq!(ids(&all), expected_order(&s, &a).await?);
    assert_eq!(all.next_after, None);
    assert_eq!(
        all.items.len(),
        7,
        "cancelled excluded, other account excluded"
    );
    assert!(ids(&all).iter().all(|i| seeded.contains(i)));
    // Running rows first, newest start first; then history, latest finish first.
    let keys: Vec<(bool, i64)> = all
        .items
        .iter()
        .map(|item| match item {
            AccountTimerSession::Available {
                started_at,
                stopped_at,
                ..
            } => (stopped_at.is_none(), stopped_at.unwrap_or(*started_at)),
            AccountTimerSession::Restricted { .. } => unreachable!("own tasks are readable"),
        })
        .collect();
    assert_eq!(
        keys,
        [
            (true, NOW - 400),
            (true, NOW - 400),
            (true, NOW - 500),
            (false, NOW - 800),
            (false, NOW - 800),
            (false, NOW - 1000),
            (false, NOW - 2000)
        ]
    );
    let owned = s
        .account_timer_sessions(&b, TimerState::All, None, 200)
        .await?;
    assert_eq!(ids(&owned), std::slice::from_ref(&other));

    // State filters.
    let running = s
        .account_timer_sessions(&a, TimerState::Running, None, 200)
        .await?;
    assert_eq!(ids(&running), ids(&all)[..3]);
    let stopped = s
        .account_timer_sessions(&a, TimerState::Stopped, None, 200)
        .await?;
    assert_eq!(ids(&stopped), ids(&all)[3..]);

    // A one-row walk visits every row once, in the same order, across the segment boundary.
    for state in [TimerState::All, TimerState::Running, TimerState::Stopped] {
        let unpaged = s.account_timer_sessions(&a, state, None, 200).await?;
        let pages = walk(&s, &a, state, 1).await?;
        assert!(pages.iter().all(|p| p.items.len() == 1));
        let walked: Vec<String> = pages.iter().flat_map(ids).collect();
        assert_eq!(walked, ids(&unpaged), "{state:?}");
        assert_eq!(pages.len(), unpaged.items.len());
        assert_eq!(pages.last().unwrap().next_after, None);
    }
    // Page sizes that do and do not divide the list; a full last page has no cursor.
    for limit in [2u16, 3, 4, 6, 7] {
        let pages = walk(&s, &a, TimerState::All, limit).await?;
        let walked: Vec<String> = pages.iter().flat_map(ids).collect();
        assert_eq!(walked, ids(&all), "limit {limit}");
        for page in &pages[..pages.len() - 1] {
            assert_eq!(page.items.len(), usize::from(limit));
            assert!(page.next_after.is_some());
        }
        assert_eq!(pages.last().unwrap().next_after, None, "limit {limit}");
    }
    assert_eq!(
        walk(&s, &a, TimerState::All, 7).await?.len(),
        1,
        "no trailing empty page"
    );
    let first = s
        .account_timer_sessions(&a, TimerState::All, None, 3)
        .await?;
    assert_eq!(first.next_after.as_deref().map(|c| &c[..2]), Some("r."));
    let second = s
        .account_timer_sessions(&a, TimerState::All, first.next_after.as_deref(), 3)
        .await?;
    assert_eq!(second.next_after.as_deref().map(|c| &c[..2]), Some("s."));
    Ok(())
}

#[tokio::test]
async fn a_cursor_survives_changes_between_pages() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    seed(&s, &a).await?;
    let first = s
        .account_timer_sessions(&a, TimerState::All, None, 2)
        .await?;
    // Before the next page: a newer timer starts and the oldest running timer stops. Keyset
    // paging neither repeats nor skips a row that did not change.
    let (o, fresh) = (task(&s, &a, seconds()).await?, id());
    start(&s, &a, &o, &fresh, NOW - 10).await?;
    let second = s
        .account_timer_sessions(&a, TimerState::All, first.next_after.as_deref(), 200)
        .await?;
    assert!(
        !ids(&second).contains(&fresh),
        "a newer start sorts before the cursor"
    );
    assert!(
        ids(&first).iter().all(|i| !ids(&second).contains(i)),
        "no repeats"
    );
    // A running timer that stops after its page was read appears again, as stopped, on a later page.
    let running: Vec<AccountTimerSession> = first.items.clone();
    let AccountTimerSession::Available {
        occurrence_id,
        id: stopping,
        ..
    } = &running[0]
    else {
        unreachable!()
    };
    stop(&s, &a, occurrence_id, stopping, NOW - 300).await?;
    let stopped = s
        .account_timer_sessions(&a, TimerState::Stopped, None, 200)
        .await?;
    assert!(ids(&stopped).contains(stopping));
    Ok(())
}

#[tokio::test]
async fn cursor_and_limit_validation() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    seed(&s, &a).await?;
    let good = id();
    let bad_cursors: Vec<String> = vec![
        String::new(),
        "x".into(),
        "r".into(),
        "r.".into(),
        "r.1".into(),
        "r.1.".into(),
        "r..1".into(),
        "r.abc.".into(),
        "q.1.".into(),
        "r.1.not-a-uuid".into(),
        "r.+1.".into(),
        "r.1_0.".into(),
        "r.12345678901234567890.".into(),
        format!("r.1.{good}{good}"),
        format!("r.1.{}", good.to_uppercase()),
        format!("r. 1.{good}"),
        format!("r.1.{good} "),
        format!("r.1e3.{good}"),
        format!("r.0x1.{good}"),
        format!("R.1.{good}"),
    ];
    for bad in &bad_cursors {
        let error = s
            .account_timer_sessions(&a, TimerState::All, Some(bad), 10)
            .await;
        assert_eq!(code(error.unwrap_err()), "invalid_value", "cursor {bad:?}");
    }
    // Numeric keys may be negative or zero; a cursor names a position, not a row that must exist.
    for good_cursor in [
        format!("r.-5.{good}"),
        format!("s.0.{good}"),
        format!("r.-999999999999999999.{good}"),
    ] {
        s.account_timer_sessions(&a, TimerState::All, Some(&good_cursor), 10)
            .await?;
    }
    // A cursor from one segment cannot resume a listing that excludes the segment.
    for (state, cursor) in [
        (TimerState::Stopped, format!("r.1.{good}")),
        (TimerState::Running, format!("s.1.{good}")),
    ] {
        let error = s.account_timer_sessions(&a, state, Some(&cursor), 10).await;
        assert_eq!(code(error.unwrap_err()), "invalid_value");
    }
    for limit in [0u16, 201, u16::MAX] {
        let error = s
            .account_timer_sessions(&a, TimerState::All, None, limit)
            .await;
        assert_eq!(code(error.unwrap_err()), "invalid_value", "limit {limit}");
    }
    for limit in [1u16, 200] {
        s.account_timer_sessions(&a, TimerState::All, None, limit)
            .await?;
    }
    // An account that no longer exists is unauthenticated, never an empty success.
    let error = s
        .account_timer_sessions(&id(), TimerState::All, None, 10)
        .await;
    assert_eq!(code(error.unwrap_err()), "unauthenticated");
    Ok(())
}

/// Negative and zero timestamps sort and page like any other key.
#[tokio::test]
async fn keys_round_trip_through_the_cursor_for_very_old_sessions() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let mut expected = Vec::new();
    for (from, to) in [(-5000, -4000), (-3000, -2000), (-100, 0)] {
        let (o, session) = (task(&s, &a, seconds()).await?, id());
        start(&s, &a, &o, &session, from).await?;
        stop(&s, &a, &o, &session, to).await?;
        expected.push((to, session));
    }
    expected.sort_by_key(|x| std::cmp::Reverse(x.0));
    let pages = walk(&s, &a, TimerState::All, 1).await?;
    let walked: Vec<String> = pages.iter().flat_map(ids).collect();
    assert_eq!(
        walked,
        expected.into_iter().map(|(_, id)| id).collect::<Vec<_>>()
    );
    assert!(pages[0].next_after.as_deref().unwrap().starts_with("s.0."));
    assert!(
        pages[1]
            .next_after
            .as_deref()
            .unwrap()
            .starts_with("s.-2000.")
    );
    Ok(())
}

async fn resource_version(s: &Store, id: &str, column: &str) -> Result<i64> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT {column} FROM resources WHERE id=$1"
    )))
    .bind(id)
    .fetch_one(&s.pool)
    .await?)
}

fn keys(item: &AccountTimerSession) -> Vec<String> {
    let Value::Object(map) = serde_json::to_value(item).unwrap() else {
        unreachable!()
    };
    let mut names: Vec<String> = map.keys().cloned().collect();
    names.sort();
    names
}

/// E9/E10/D6/D7: access narrows and widens under a live list. Inaccessible rows stay listed with
/// exactly four members; nothing else about the task is exposed; nothing is stopped for anyone.
#[tokio::test]
async fn inaccessible_timers_are_redacted_never_dropped_and_access_restores_detail() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let b = account(&s).await?;
    let mut d = definition("2026-09-08", Carry::CloseIncomplete, Participation::Anyone);
    d.goal = seconds();
    let task_id = create(&s, &a, d, Some(shared(&b, true)), at(8)).await?;
    let shared_occurrence = view(&s, &a, &task_id, at(8)).await?[0].id.clone();
    let now = at(8);
    // B times the shared occurrence twice (one finished, one running) and also owns a private task.
    let (finished, running, private_session) = (id(), id(), id());
    let mine = task(&s, &b, seconds()).await?;
    for (session, from, to) in [
        (&finished, now - 900, Some(now - 800)),
        (&running, now - 500, None),
    ] {
        s.task_command(
            &b,
            &id(),
            &TaskCommand::StartTimer {
                occurrence_id: shared_occurrence.clone(),
                session_id: session.clone(),
                started_at: from,
            },
            None,
            now,
        )
        .await?;
        if let Some(to) = to {
            s.task_command(
                &b,
                &id(),
                &TaskCommand::StopTimer {
                    occurrence_id: shared_occurrence.clone(),
                    session_id: session.clone(),
                    expected_version: 1,
                    stopped_at: to,
                },
                None,
                now,
            )
            .await?;
        }
    }
    s.task_command(
        &b,
        &id(),
        &TaskCommand::StartTimer {
            occurrence_id: mine.clone(),
            session_id: private_session.clone(),
            started_at: now - 100,
        },
        None,
        now,
    )
    .await?;

    let page = s
        .account_timer_sessions(&b, TimerState::All, None, 50)
        .await?;
    assert_eq!(page.items.len(), 3);
    for item in &page.items {
        let AccountTimerSession::Available {
            task_title,
            task_id: listed,
            occurrence_id,
            can_modify,
            stopped_at,
            ..
        } = item
        else {
            panic!("everything is readable: {item:?}")
        };
        assert_eq!(task_title, "Task");
        assert_eq!(
            *can_modify,
            stopped_at.is_none(),
            "only running sessions can be modified"
        );
        if listed == &task_id {
            assert_eq!(occurrence_id, &shared_occurrence);
        }
    }
    assert_eq!(
        keys(&page.items[0]),
        [
            "access",
            "can_modify",
            "id",
            "occurrence_id",
            "slot_date",
            "started_at",
            "stopped_at",
            "task_id",
            "task_title",
            "version"
        ]
    );

    // A withdraws B's access to the task: B's rows on it become restricted, B's own task stays.
    let revoke = |version: i64| Command::Revoke {
        id: task_id.clone(),
        expected_version: version,
        account_id: b.clone(),
    };
    s.apply(
        &a,
        &id(),
        &[revoke(
            resource_version(&s, &task_id, "policy_version").await?,
        )],
    )
    .await?;
    let narrowed = s
        .account_timer_sessions(&b, TimerState::All, None, 50)
        .await?;
    assert_eq!(narrowed.items.len(), 3, "restricted rows are still listed");
    assert_eq!(ids(&narrowed), ids(&page), "order and membership unchanged");
    let restricted: Vec<&AccountTimerSession> = narrowed
        .items
        .iter()
        .filter(|i| matches!(i, AccountTimerSession::Restricted { .. }))
        .collect();
    assert_eq!(restricted.len(), 2, "one running, one stopped");
    for item in restricted {
        assert_eq!(
            keys(item),
            ["access", "id", "started_at", "stopped_at", "version"]
        );
        let text = serde_json::to_string(item)?;
        for leaked in [
            task_id.as_str(),
            shared_occurrence.as_str(),
            "Task",
            "slot_date",
        ] {
            assert!(!text.contains(leaked), "{text} leaks {leaked}");
        }
    }
    // Paging behaves the same for restricted rows, and the state filters still see them.
    let walked: Vec<String> = walk(&s, &b, TimerState::All, 1)
        .await?
        .iter()
        .flat_map(ids)
        .collect();
    assert_eq!(walked, ids(&page));
    assert_eq!(
        s.account_timer_sessions(&b, TimerState::Stopped, None, 50)
            .await?
            .items
            .len(),
        1
    );
    // D7: the owner cannot stop what they can no longer read, and it blocks no other timer.
    let stop_it = TaskCommand::StopTimer {
        occurrence_id: shared_occurrence.clone(),
        session_id: running.clone(),
        expected_version: 1,
        stopped_at: now - 50,
    };
    assert_eq!(
        code(
            s.task_command(&b, &id(), &stop_it, None, now)
                .await
                .unwrap_err()
        ),
        "not_found"
    );
    let another = task(&s, &b, seconds()).await?;
    s.task_command(
        &b,
        &id(),
        &TaskCommand::StartTimer {
            occurrence_id: another,
            session_id: id(),
            started_at: now - 60,
        },
        None,
        now,
    )
    .await?;
    // Nothing was stopped or discarded on the owner's behalf.
    let still_running: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM timer_sessions WHERE account_id=$1 AND stopped_at IS NULL AND cancelled=0")
        .bind(&b)
        .fetch_one(&s.pool)
        .await?;
    assert_eq!(still_running, 3);
    // A cannot see B's timers at all.
    assert!(
        s.account_timer_sessions(&a, TimerState::All, None, 50)
            .await?
            .items
            .is_empty()
    );

    // Access restored: detail returns.
    let grant = Command::Grant {
        id: task_id.clone(),
        expected_version: resource_version(&s, &task_id, "policy_version").await?,
        account_id: b.clone(),
        edit: false,
    };
    s.apply(&a, &id(), &[grant]).await?;
    let widened = s
        .account_timer_sessions(&b, TimerState::All, None, 50)
        .await?;
    assert_eq!(widened.items.len(), 4);
    assert!(
        widened
            .items
            .iter()
            .all(|i| matches!(i, AccountTimerSession::Available { .. }))
    );
    // Read-only access to the task does not change what the owner may do to their own stream:
    // the flag reports the same authority the stop command applies.
    let AccountTimerSession::Available { can_modify, .. } = widened
        .items
        .iter()
        .find(|i| session_id(i) == running)
        .unwrap()
    else {
        unreachable!()
    };
    let stopped_now = s.task_command(&b, &id(), &stop_it, None, now).await;
    assert_eq!(
        *can_modify,
        stopped_now.is_ok(),
        "can_modify agrees with the command"
    );
    Ok(())
}

/// `can_modify` is exactly the authority the stop and discard commands apply to a running session.
#[tokio::test]
async fn can_modify_reports_the_command_authority() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let (o, session) = (task(&s, &a, seconds()).await?, id());
    start(&s, &a, &o, &session, NOW - 300).await?;
    let flag = |page: &AccountTimerPage| match &page.items[0] {
        AccountTimerSession::Available { can_modify, .. } => *can_modify,
        other => panic!("{other:?}"),
    };
    let page = s
        .account_timer_sessions(&a, TimerState::All, None, 10)
        .await?;
    assert!(flag(&page));
    // An occurrence covered by another is refused by the commands (invalid_value), so it is not
    // offered: the row is still available, but `can_modify` is false.
    let mut occurrence: Value = serde_json::from_str(
        &sqlx::query_scalar::<_, String>("SELECT value FROM resources WHERE id=$1")
            .bind(&o)
            .fetch_one(&s.pool)
            .await?,
    )?;
    occurrence["covered_by"] = Value::String(id());
    sqlx::query("UPDATE resources SET value=$1 WHERE id=$2")
        .bind(occurrence.to_string())
        .bind(&o)
        .execute(&s.pool)
        .await?;
    let page = s
        .account_timer_sessions(&a, TimerState::All, None, 10)
        .await?;
    assert!(!flag(&page));
    let stop_it = TaskCommand::StopTimer {
        occurrence_id: o.clone(),
        session_id: session,
        expected_version: 1,
        stopped_at: NOW - 100,
    };
    assert_eq!(
        code(
            s.task_command(&a, &id(), &stop_it, None, NOW)
                .await
                .unwrap_err()
        ),
        "invalid_value"
    );
    Ok(())
}

/// E10: a session whose participant link is missing is restricted, not an error or a panic.
#[tokio::test]
async fn a_session_with_a_missing_participant_link_is_restricted() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let (o, session) = (task(&s, &a, seconds()).await?, id());
    start(&s, &a, &o, &session, NOW - 300).await?;
    sqlx::query("DELETE FROM occurrence_participants WHERE occurrence_id=$1 AND account_id=$2")
        .bind(&o)
        .bind(&a)
        .execute(&s.pool)
        .await?;
    let page = s
        .account_timer_sessions(&a, TimerState::All, None, 10)
        .await?;
    assert_eq!(page.items.len(), 1);
    assert!(matches!(&page.items[0], AccountTimerSession::Restricted { id, .. } if id == &session));
    Ok(())
}
