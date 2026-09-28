use anyhow::Result;
use atlas_core::{Store, tasks::*};
use std::time::Duration;

const NOW: i64 = 1788868800;
use crate::support::task_workflows::{account, id, setup, task};

async fn timer_scenario(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let b = account(s).await?;
    let o = task(
        s,
        &a,
        Goal::Numeric {
            minimum: Some(Decimal("60".into())),
            maximum: None,
            unit: "seconds".into(),
        },
    )
    .await?;
    let session = id();
    let operation = id();
    let start = TaskCommand::StartTimer {
        occurrence_id: o.clone(),
        session_id: session.clone(),
        started_at: NOW - 120,
    };
    s.task_command(&a, &operation, &start, None, NOW).await?;
    s.task_command(&a, &operation, &start, None, NOW).await?;
    assert_eq!(s.timer_sessions(&a, &o, None, 50).await?.len(), 1);
    assert!(s.timer_sessions(&b, &o, None, 50).await.is_err());
    assert!(
        s.task_command(
            &a,
            &id(),
            &TaskCommand::StartTimer {
                occurrence_id: o.clone(),
                session_id: id(),
                started_at: NOW
            },
            None,
            NOW
        )
        .await
        .is_err()
    );
    let stop = TaskCommand::StopTimer {
        occurrence_id: o.clone(),
        session_id: session.clone(),
        expected_version: 1,
        stopped_at: NOW - 60,
    };
    assert!(s.task_command(&b, &id(), &stop, None, NOW).await.is_err());
    let operation = id();
    s.task_command(&a, &operation, &stop, None, NOW).await?;
    s.task_command(&a, &operation, &stop, None, NOW).await?;
    let progress = s
        .resources(&a, "progress", Some(&o), false, None, 50)
        .await?;
    let state: ProgressState = serde_json::from_value(progress[0].value.clone())?;
    assert_eq!(state.action.amount, Some(Decimal("60".into())));
    assert_eq!(state.entry_count, 1);
    // A delayed offline session starts before an existing interval. Stopping it
    // across that interval must roll back; it can then be cancelled to recover.
    let delayed = id();
    s.task_command(
        &a,
        &id(),
        &TaskCommand::StartTimer {
            occurrence_id: o.clone(),
            session_id: delayed.clone(),
            started_at: NOW - 180,
        },
        None,
        NOW,
    )
    .await?;
    assert!(
        s.task_command(
            &a,
            &id(),
            &TaskCommand::StopTimer {
                occurrence_id: o.clone(),
                session_id: delayed.clone(),
                expected_version: 1,
                stopped_at: NOW
            },
            None,
            NOW
        )
        .await
        .is_err()
    );
    s.task_command(
        &a,
        &id(),
        &TaskCommand::CancelTimer {
            occurrence_id: o.clone(),
            session_id: delayed,
            expected_version: 1,
        },
        None,
        NOW,
    )
    .await?;
    assert_eq!(s.timer_sessions(&a, &o, None, 50).await?.len(), 1);
    // A touching interval is valid and contributes once.
    let resumed = id();
    s.task_command(
        &a,
        &id(),
        &TaskCommand::StartTimer {
            occurrence_id: o.clone(),
            session_id: resumed.clone(),
            started_at: NOW - 60,
        },
        None,
        NOW,
    )
    .await?;
    s.task_command(
        &a,
        &id(),
        &TaskCommand::StopTimer {
            occurrence_id: o.clone(),
            session_id: resumed,
            expected_version: 1,
            stopped_at: NOW,
        },
        None,
        NOW,
    )
    .await?;
    let progress = s
        .resources(&a, "progress", Some(&o), false, None, 50)
        .await?;
    let state: ProgressState = serde_json::from_value(progress[0].value.clone())?;
    assert_eq!(state.action.amount, Some(Decimal("120".into())));
    assert_eq!(state.entry_count, 2);
    Ok(())
}

#[tokio::test]
async fn timers_overlap_replay_and_authority() -> Result<()> {
    let (s, _dir) = setup().await?;
    timer_scenario(&s).await
}

// --- BE-B6: timers are independent per person per occurrence --------------------------------

fn seconds() -> Goal {
    Goal::Numeric {
        minimum: Some(Decimal("60".into())),
        maximum: None,
        unit: "seconds".into(),
    }
}

async fn start(s: &Store, a: &str, o: &str, session: &str, at: i64) -> Result<TaskResult> {
    s.task_command(
        a,
        &id(),
        &TaskCommand::StartTimer {
            occurrence_id: o.into(),
            session_id: session.into(),
            started_at: at,
        },
        None,
        NOW,
    )
    .await
}

async fn stop(s: &Store, a: &str, o: &str, session: &str, at: i64) -> Result<TaskResult> {
    s.task_command(
        a,
        &id(),
        &TaskCommand::StopTimer {
            occurrence_id: o.into(),
            session_id: session.into(),
            expected_version: 1,
            stopped_at: at,
        },
        None,
        NOW,
    )
    .await
}

async fn recorded(s: &Store, a: &str, o: &str) -> Result<(Option<Decimal>, usize)> {
    let progress = s.resources(a, "progress", Some(o), false, None, 50).await?;
    let state: ProgressState = serde_json::from_value(progress[0].value.clone())?;
    Ok((state.action.amount, state.entry_count))
}

fn code(error: anyhow::Error) -> String {
    error.to_string()
}

/// The progress stream a person's timers on `occurrence` are recorded against.
async fn stream_of(s: &Store, a: &str, o: &str) -> Result<String> {
    Ok(sqlx::query_scalar(
        "SELECT progress_id FROM occurrence_participants WHERE occurrence_id=$1 AND account_id=$2",
    )
    .bind(o)
    .bind(a)
    .fetch_one(&s.pool)
    .await?)
}

/// E1/E4: three occurrences whose intervals nest and partly overlap. Every arrival order of the
/// starts and of the stops succeeds, and each occurrence records exactly its own duration.
#[tokio::test]
async fn timers_on_different_occurrences_overlap_in_every_arrival_order() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    // Intervals (seconds before NOW): X [900,300] holds Y [700,500]; Z [400,100] overlaps X's end.
    let intervals = [(900, 300), (700, 500), (400, 100)];
    let orders = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    for starts in orders {
        for stops in [orders[0], orders[5], orders[(starts[0] + 1) % 6]] {
            let mut occurrences = Vec::new();
            for _ in 0..3 {
                occurrences.push(task(&s, &a, seconds()).await?);
            }
            let sessions = [id(), id(), id()];
            for i in starts {
                start(&s, &a, &occurrences[i], &sessions[i], NOW - intervals[i].0).await?;
            }
            // All three run at once, one per occurrence.
            let running: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM timer_sessions WHERE account_id=$1 AND stopped_at IS NULL AND cancelled=0 AND id IN ($2,$3,$4)",
            )
            .bind(&a)
            .bind(&sessions[0])
            .bind(&sessions[1])
            .bind(&sessions[2])
            .fetch_one(&s.pool)
            .await?;
            assert_eq!(running, 3, "starts {starts:?}");
            for i in stops {
                stop(&s, &a, &occurrences[i], &sessions[i], NOW - intervals[i].1).await?;
            }
            for i in 0..3 {
                let (amount, entries) = recorded(&s, &a, &occurrences[i]).await?;
                let expected = intervals[i].0 - intervals[i].1;
                assert_eq!(
                    amount,
                    Some(Decimal(expected.to_string())),
                    "own duration, starts {starts:?} stops {stops:?}"
                );
                assert_eq!(entries, 1);
            }
        }
    }
    Ok(())
}

/// E2/E3: the rule that remains. One running timer per person per occurrence, and a person's own
/// intervals on one occurrence never overlap; the same answer as before, for the narrower scope.
#[tokio::test]
async fn the_same_occurrence_still_allows_one_timer_at_a_time() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let one = task(&s, &a, seconds()).await?;
    let other = task(&s, &a, seconds()).await?;
    let first = id();
    start(&s, &a, &one, &first, NOW - 600).await?;
    // A second running timer on the same occurrence, from any device, is a conflict.
    assert_eq!(
        code(start(&s, &a, &one, &id(), NOW - 500).await.unwrap_err()),
        "conflict"
    );
    // Another occurrence is unaffected, before and after the conflict.
    let independent = id();
    start(&s, &a, &other, &independent, NOW - 550).await?;
    stop(&s, &a, &one, &first, NOW - 300).await?;
    // A start inside the stopped interval of the same occurrence is still a conflict...
    assert_eq!(
        code(start(&s, &a, &one, &id(), NOW - 400).await.unwrap_err()),
        "conflict"
    );
    // ...and stopping a timer across it is too, while the other occurrence's timer is untouched.
    let late = id();
    start(&s, &a, &one, &late, NOW - 700).await?;
    assert_eq!(
        code(stop(&s, &a, &one, &late, NOW - 200).await.unwrap_err()),
        "conflict"
    );
    stop(&s, &a, &other, &independent, NOW - 100).await?;
    assert_eq!(
        recorded(&s, &a, &other).await?.0,
        Some(Decimal("450".into()))
    );
    // A touching interval on the same occurrence is valid.
    stop(&s, &a, &one, &late, NOW - 600).await?;
    assert_eq!(recorded(&s, &a, &one).await?.1, 2);
    Ok(())
}

/// E5: an offline queue replayed after partial delivery, then replayed in full again. Receipts
/// answer the repeat with the original result and create nothing; a changed body is refused.
#[tokio::test]
async fn an_offline_queue_of_independent_timers_replays_exactly_once() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let x = task(&s, &a, seconds()).await?;
    let y = task(&s, &a, seconds()).await?;
    let (sx, sy) = (id(), id());
    let queue = [
        (
            id(),
            TaskCommand::StartTimer {
                occurrence_id: x.clone(),
                session_id: sx.clone(),
                started_at: NOW - 900,
            },
        ),
        (
            id(),
            TaskCommand::StartTimer {
                occurrence_id: y.clone(),
                session_id: sy.clone(),
                started_at: NOW - 800,
            },
        ),
        (
            id(),
            TaskCommand::StopTimer {
                occurrence_id: y.clone(),
                session_id: sy.clone(),
                expected_version: 1,
                stopped_at: NOW - 200,
            },
        ),
        (
            id(),
            TaskCommand::StopTimer {
                occurrence_id: x.clone(),
                session_id: sx.clone(),
                expected_version: 1,
                stopped_at: NOW - 100,
            },
        ),
    ];
    let mut revisions = Vec::new();
    // The first two commands reached the server before the connection dropped; the whole queue is
    // then submitted, in order, with the same operation identifiers.
    for (operation, command) in &queue[..2] {
        revisions.push(
            s.task_command(&a, operation, command, None, NOW)
                .await?
                .revision,
        );
    }
    for (index, (operation, command)) in queue.iter().enumerate() {
        let result = s.task_command(&a, operation, command, None, NOW).await?;
        if index < 2 {
            assert_eq!(
                result.revision, revisions[index],
                "already delivered: original receipt"
            );
        } else {
            revisions.push(result.revision);
        }
    }
    let rows = |s: &Store| {
        let s = s.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM timer_sessions")
                .fetch_one(&s.pool)
                .await
        }
    };
    let (sessions, entries) = (rows(&s).await?, count_entries(&s).await?);
    assert_eq!((sessions, entries), (2, 2));
    // A complete second replay changes nothing and returns the originals.
    for (index, (operation, command)) in queue.iter().enumerate() {
        let again = s.task_command(&a, operation, command, None, NOW).await?;
        assert_eq!(again.revision, revisions[index]);
    }
    assert_eq!((rows(&s).await?, count_entries(&s).await?), (2, 2));
    assert_eq!(recorded(&s, &a, &x).await?.0, Some(Decimal("800".into())));
    assert_eq!(recorded(&s, &a, &y).await?.0, Some(Decimal("600".into())));
    // The same operation identifier with a different body is refused, never reinterpreted.
    let changed = TaskCommand::StartTimer {
        occurrence_id: x,
        session_id: sx,
        started_at: NOW - 901,
    };
    assert_eq!(
        code(
            s.task_command(&a, &queue[0].0, &changed, None, NOW)
                .await
                .unwrap_err()
        ),
        "operation_conflict"
    );
    Ok(())
}

async fn count_entries(s: &Store) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM progress_entries")
        .fetch_one(&s.pool)
        .await?)
}

/// E6: two people timing one shared occurrence each have their own stream and timer.
#[tokio::test]
async fn two_people_time_the_same_shared_occurrence_independently() -> Result<()> {
    use crate::support::tasks::{at, create, definition, shared, view};
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let b = account(&s).await?;
    let mut d = definition("2026-09-08", Carry::CloseIncomplete, Participation::Anyone);
    d.goal = seconds();
    let task = create(&s, &a, d, Some(shared(&b, true)), at(8)).await?;
    let occurrence = view(&s, &a, &task, at(8)).await?[0].id.clone();
    let now = at(8);
    let (sa, sb) = (id(), id());
    for (who, session, from) in [(&a, &sa, now - 900), (&b, &sb, now - 800)] {
        s.task_command(
            who,
            &id(),
            &TaskCommand::StartTimer {
                occurrence_id: occurrence.clone(),
                session_id: session.clone(),
                started_at: from,
            },
            None,
            now,
        )
        .await?;
    }
    assert_ne!(
        stream_of(&s, &a, &occurrence).await?,
        stream_of(&s, &b, &occurrence).await?
    );
    assert_eq!(s.timer_sessions(&a, &occurrence, None, 10).await?.len(), 1);
    assert_eq!(s.timer_sessions(&b, &occurrence, None, 10).await?.len(), 1);
    // B cannot stop A's timer through B's own stream.
    assert!(
        s.task_command(
            &b,
            &id(),
            &TaskCommand::StopTimer {
                occurrence_id: occurrence.clone(),
                session_id: sa.clone(),
                expected_version: 1,
                stopped_at: now - 100,
            },
            None,
            now,
        )
        .await
        .is_err()
    );
    for (who, session, until) in [(&b, &sb, now - 200), (&a, &sa, now - 100)] {
        s.task_command(
            who,
            &id(),
            &TaskCommand::StopTimer {
                occurrence_id: occurrence.clone(),
                session_id: session.clone(),
                expected_version: 1,
                stopped_at: until,
            },
            None,
            now,
        )
        .await?;
    }
    assert_eq!(
        recorded(&s, &a, &occurrence).await?.0,
        Some(Decimal("800".into()))
    );
    assert_eq!(
        recorded(&s, &b, &occurrence).await?.0,
        Some(Decimal("600".into()))
    );
    Ok(())
}

/// E7: cancelling frees the occurrence's slot; the session id stays reserved.
#[tokio::test]
async fn cancelling_a_timer_frees_only_its_occurrence() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let x = task(&s, &a, seconds()).await?;
    let y = task(&s, &a, seconds()).await?;
    let (first, kept) = (id(), id());
    start(&s, &a, &x, &first, NOW - 300).await?;
    start(&s, &a, &y, &kept, NOW - 300).await?;
    s.task_command(
        &a,
        &id(),
        &TaskCommand::CancelTimer {
            occurrence_id: x.clone(),
            session_id: first.clone(),
            expected_version: 1,
        },
        None,
        NOW,
    )
    .await?;
    // The slot is free again; the discarded session's id is not reusable; the other is running.
    start(&s, &a, &x, &id(), NOW - 200).await?;
    assert!(start(&s, &a, &x, &first, NOW - 100).await.is_err());
    assert_eq!(
        s.timer_sessions(&a, &y, None, 10).await?[0].stopped_at,
        None
    );
    assert_eq!(s.timer_sessions(&a, &y, None, 10).await?[0].id, kept);
    Ok(())
}

/// E8: concurrent starts. Different occurrences never conflict and never deadlock; the same
/// occurrence has exactly one winner.
#[tokio::test]
async fn concurrent_starts_scale_by_occurrence() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let mut occurrences = Vec::new();
    for _ in 0..8 {
        occurrences.push(task(&s, &a, seconds()).await?);
    }
    let mut jobs = tokio::task::JoinSet::new();
    for occurrence in &occurrences {
        let (store, actor, occurrence) = (s.clone(), a.clone(), occurrence.clone());
        jobs.spawn(async move { start(&store, &actor, &occurrence, &id(), NOW - 300).await });
    }
    while let Some(done) = jobs.join_next().await {
        done??;
    }
    let running: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM timer_sessions WHERE account_id=$1 AND stopped_at IS NULL AND cancelled=0",
    )
    .bind(&a)
    .fetch_one(&s.pool)
    .await?;
    assert_eq!(running, 8);
    // Racing starts for one occurrence: exactly one wins, the rest are conflicts.
    let contested = task(&s, &a, seconds()).await?;
    let mut jobs = tokio::task::JoinSet::new();
    for _ in 0..6 {
        let (store, actor, occurrence) = (s.clone(), a.clone(), contested.clone());
        jobs.spawn(async move { start(&store, &actor, &occurrence, &id(), NOW - 200).await });
    }
    let (mut won, mut lost) = (0, 0);
    while let Some(done) = jobs.join_next().await {
        match done? {
            Ok(_) => won += 1,
            Err(error) => {
                assert_eq!(code(error), "conflict");
                lost += 1;
            }
        }
    }
    assert_eq!((won, lost), (1, 5));
    Ok(())
}

/// The serialisation seam: `begin_serial` is the gate every command takes. A start held open on
/// one occurrence blocks a concurrent start on another until it commits, after which the second
/// succeeds (it is not a conflict); the same schedule on the *same* occurrence ends in a conflict.
#[tokio::test]
async fn a_held_start_serialises_but_only_conflicts_on_its_own_occurrence() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let (x, y, z) = (
        task(&s, &a, seconds()).await?,
        task(&s, &a, seconds()).await?,
        task(&s, &a, seconds()).await?,
    );
    for (held_on, contender_on, expect_conflict) in [(&x, &y, false), (&z, &z, true)] {
        let held_session = id();
        let tx = s.begin_serial().await?;
        let (_, tx) = Store::task_command_on(
            tx,
            &a,
            &id(),
            &TaskCommand::StartTimer {
                occurrence_id: held_on.clone(),
                session_id: held_session.clone(),
                started_at: NOW - 300,
            },
            None,
            NOW,
        )
        .await?;
        let (store, actor, occurrence) = (s.clone(), a.clone(), contender_on.clone());
        let contender =
            tokio::spawn(async move { start(&store, &actor, &occurrence, &id(), NOW - 200).await });
        // The contender is parked behind the gate: not finished after a bounded wait.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!contender.is_finished(), "the gate serialises commands");
        if crate::support::database::postgres() {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity WHERE datname=current_database() AND cardinality(pg_blocking_pids(pid))>0",
            )
            .fetch_one(&s.pool)
            .await?;
            assert!(waiting >= 1, "a backend is blocked on the holder");
        }
        tx.commit().await?;
        let result = tokio::time::timeout(Duration::from_secs(20), contender).await??;
        match (expect_conflict, result) {
            (false, Ok(_)) => {}
            (true, Err(error)) => assert_eq!(code(error), "conflict"),
            (false, Err(error)) => panic!("independent occurrence must not conflict: {error}"),
            (true, Ok(_)) => panic!("same occurrence must conflict"),
        }
        // Nothing is left running: the held session is stopped, and so is the contender's if it won.
        stop(&s, &a, held_on, &held_session, NOW - 250).await?;
        if !expect_conflict {
            let other = s.timer_sessions(&a, contender_on, None, 10).await?;
            stop(&s, &a, contender_on, &other[0].id, NOW - 100).await?;
        }
    }
    Ok(())
}

/// The database enforces the rule independently of the application query: a second running row
/// for the same person and occurrence is a unique violation whatever path inserts it, while a
/// running row on another occurrence commits.
#[tokio::test]
async fn the_database_allows_one_running_row_per_person_and_occurrence() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let (x, y) = (
        task(&s, &a, seconds()).await?,
        task(&s, &a, seconds()).await?,
    );
    start(&s, &a, &x, &id(), NOW - 300).await?;
    let (px, py) = (stream_of(&s, &a, &x).await?, stream_of(&s, &a, &y).await?);
    let insert = |progress: String, id: String| {
        let s = s.clone();
        let a = a.clone();
        async move {
            sqlx::query("INSERT INTO timer_sessions(id,progress_id,account_id,started_at,version) VALUES ($1,$2,$3,$4,1)")
                .bind(id)
                .bind(progress)
                .bind(a)
                .bind(NOW - 100)
                .execute(&s.pool)
                .await
        }
    };
    // Another occurrence: allowed (this was rejected by the account-wide index before 1004).
    insert(py.clone(), id()).await?;
    let error = insert(px.clone(), id()).await.unwrap_err();
    assert!(
        error
            .as_database_error()
            .is_some_and(|e| e.is_unique_violation()),
        "{error}"
    );
    // Stopped and cancelled rows are outside the partial index: history never collides.
    sqlx::query("UPDATE timer_sessions SET stopped_at=$1,version=2 WHERE progress_id=$2 AND stopped_at IS NULL")
        .bind(NOW)
        .bind(&py)
        .execute(&s.pool)
        .await?;
    insert(py, id()).await?;
    Ok(())
}

/// PostgreSQL: two concurrent transactions inserting the same person/occurrence. The second waits
/// on the first's uncommitted row, then fails with a unique violation once it commits.
#[tokio::test]
async fn concurrent_raw_inserts_for_one_occurrence_cannot_both_commit() -> Result<()> {
    if !crate::support::database::postgres() {
        return Ok(());
    }
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let x = task(&s, &a, seconds()).await?;
    let progress = stream_of(&s, &a, &x).await?;
    let sql = "INSERT INTO timer_sessions(id,progress_id,account_id,started_at,version) VALUES ($1,$2,$3,$4,1)";
    let mut first = s.pool.begin().await?;
    sqlx::query(sql)
        .bind(id())
        .bind(&progress)
        .bind(&a)
        .bind(NOW - 100)
        .execute(&mut *first)
        .await?;
    let (store, second_progress, actor) = (s.clone(), progress.clone(), a.clone());
    let second = tokio::spawn(async move {
        let mut tx = store.pool.begin().await?;
        sqlx::query(sql)
            .bind(id())
            .bind(&second_progress)
            .bind(&actor)
            .bind(NOW - 90)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>(())
    });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !second.is_finished(),
        "the second insert waits on the uncommitted row"
    );
    first.commit().await?;
    let error = tokio::time::timeout(Duration::from_secs(20), second)
        .await??
        .unwrap_err();
    assert!(
        error
            .as_database_error()
            .is_some_and(|e| e.is_unique_violation()),
        "{error}"
    );
    Ok(())
}
