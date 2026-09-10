use anyhow::Result;
use atlas_core::{
    Store,
    policy::{Policy, PrincipalGrant},
    tasks::*,
};

const NOW: i64 = 1788868800;
use crate::support::task_workflows::{account, id, setup};

async fn rota_scenario(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let b = account(s).await?;
    let reader = account(s).await?;
    let t = id();
    let mut definition = presets("2026-09-08", "Europe/Vienna")?[1]
        .definition
        .clone();
    definition.carry = Carry::Accumulate;
    s.task_command(
        &a,
        &id(),
        &TaskCommand::CreateTask {
            id: t.clone(),
            execution_id: id(),
            title: "Bins rota".into(),
            definition,
            anchor: None,
            initial_policy: Some(Policy {
                grants: vec![
                    PrincipalGrant::Account {
                        id: b.clone(),
                        edit: true,
                    },
                    PrincipalGrant::Account {
                        id: reader.clone(),
                        edit: false,
                    },
                ],
                exclude_accounts: vec![],
            }),
        },
        None,
        NOW,
    )
    .await?;
    let config = TaskCommand::SetRota {
        task_id: t.clone(),
        expected_version: 0,
        participants: vec![a.clone(), b.clone()],
    };
    assert!(s.task_command(&a, &id(), &config, None, NOW).await.is_err());
    s.task_command(
        &b,
        &id(),
        &TaskCommand::Enrol {
            task_id: t.clone(),
            expected_version: 0,
            active: true,
            aggregate_consent: true,
            initial_policy: None,
        },
        None,
        NOW,
    )
    .await?;
    for actor in [&a, &b] {
        s.task_command(
            actor,
            &id(),
            &TaskCommand::RotaConsent {
                task_id: t.clone(),
                expected_version: 0,
                accepted: true,
            },
            None,
            NOW,
        )
        .await?;
    }
    let eligible = s.task_rota(&a, &t).await?.eligible_participants;
    assert!(eligible.contains(&a) && eligible.contains(&b));
    assert!(
        s.task_rota(&reader, &t)
            .await?
            .eligible_participants
            .is_empty()
    );
    let op = id();
    s.task_command(&a, &op, &config, None, NOW).await?;
    s.task_command(&a, &op, &config, None, NOW).await?;
    let state = s.task_rota(&a, &t).await?;
    assert_eq!(state.version, 1);
    s.task_command(
        &a,
        &id(),
        &TaskCommand::Materialise {
            task_id: t.clone(),
            through_date: "2026-09-10".into(),
            limit: 16,
        },
        None,
        NOW,
    )
    .await?;
    let rows:Vec<String>=sqlx::query_scalar("SELECT r.value FROM task_occurrences o JOIN resources r ON r.id=o.id WHERE o.task_id=$1 ORDER BY o.slot_key").bind(&t).fetch_all(&s.pool).await?;
    let occurrences = rows
        .iter()
        .map(|v| serde_json::from_str::<OccurrenceData>(v))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(occurrences.len(), 3);
    assert!(occurrences[0].rota.is_none());
    assert_eq!(
        occurrences[1].rota.as_ref().unwrap().account_id.as_deref(),
        Some(a.as_str())
    );
    assert_eq!(
        occurrences[2].rota.as_ref().unwrap().account_id.as_deref(),
        Some(b.as_str())
    );
    // A new roster starts a fresh revision, not a rewrite of already-issued work.
    s.task_command(
        &a,
        &id(),
        &TaskCommand::SetRota {
            task_id: t.clone(),
            expected_version: 1,
            participants: vec![b.clone(), a.clone()],
        },
        None,
        NOW,
    )
    .await?;
    s.task_command(
        &b,
        &id(),
        &TaskCommand::RotaConsent {
            task_id: t.clone(),
            expected_version: 1,
            accepted: false,
        },
        None,
        NOW,
    )
    .await?;
    s.task_command(
        &a,
        &id(),
        &TaskCommand::Materialise {
            task_id: t.clone(),
            through_date: "2026-09-11".into(),
            limit: 16,
        },
        None,
        NOW,
    )
    .await?;
    let new_rows:Vec<String>=sqlx::query_scalar("SELECT r.value FROM task_occurrences o JOIN resources r ON r.id=o.id WHERE o.task_id=$1 ORDER BY o.slot_key").bind(&t).fetch_all(&s.pool).await?;
    assert_eq!(&new_rows[..3], &rows);
    let new: OccurrenceData = serde_json::from_str(&new_rows[3])?;
    let assignment = new.rota.unwrap();
    assert_eq!(assignment.revision, 2);
    assert!(assignment.account_id.is_none());
    // Retrying materialisation does not consume another turn in the rota.
    s.task_command(
        &a,
        &id(),
        &TaskCommand::Materialise {
            task_id: t.clone(),
            through_date: "2026-09-11".into(),
            limit: 16,
        },
        None,
        NOW - 3600,
    )
    .await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT next_ordinal FROM task_rotas WHERE task_id=$1")
            .bind(&t)
            .fetch_one(&s.pool)
            .await?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn rotas_require_consent_and_preserve_assignments() -> Result<()> {
    let (s, _dir) = setup().await?;
    rota_scenario(&s).await
}
