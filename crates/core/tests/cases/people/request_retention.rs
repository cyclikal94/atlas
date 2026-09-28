use anyhow::Result;
use atlas_core::{Store, people::PeopleCommand};

const NOW: i64 = 1788868800;

use crate::support::task_fixtures::{account, id, person, setup};

async fn request_cleanup(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let b = account(s).await?;
    let p = person(s, &a).await?;
    let request = id();
    let operation = id();
    let command = PeopleCommand::RequestLink {
        id: request.clone(),
        person_id: p.clone(),
        account_id: b.clone(),
        expected_version: 1,
    };
    let before = s.people_command(&a, &operation, &command, NOW).await?;
    let response_id = id();
    let response = PeopleCommand::RespondRequest {
        id: request.clone(),
        accept: false,
        recipient_preview_token: None,
    };
    let declined = s.people_command(&b, &response_id, &response, NOW).await?;
    s.collect_expired(NOW + 604800).await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM people_requests WHERE id=$1")
            .bind(&request)
            .fetch_one(&s.pool)
            .await?,
        0
    );
    let replay = s
        .people_command(&a, &operation, &command, NOW + 604801)
        .await?;
    assert_eq!(
        (replay.revision, replay.person_id),
        (before.revision, before.person_id)
    );
    let replay = s
        .people_command(&b, &response_id, &response, NOW + 604801)
        .await?;
    assert_eq!(
        (replay.revision, replay.person_id),
        (declined.revision, declined.person_id)
    );
    assert!(
        s.people_command(&a, &id(), &command, NOW + 604801)
            .await
            .is_err(),
        "collected request IDs cannot be reused"
    );
    assert!(
        s.people_command(
            &b,
            &id(),
            &PeopleCommand::RespondRequest {
                id: request,
                accept: true,
                recipient_preview_token: None
            },
            NOW + 604801
        )
        .await
        .is_err()
    );
    assert!(s.person_detail(&a, &p).await?.linked_account_id.is_none());
    Ok(())
}

#[tokio::test]
async fn expired_requests_release_payloads_but_preserve_replay() -> Result<()> {
    let (s, _d) = setup().await?;
    request_cleanup(&s).await
}

/// BE-Q16: a request never responded to before its `expires_at` is purged from `people_requests`
/// by `collect_expired`, exactly like today — but `people_request_history` (never swept) still
/// reports it, with the `pending -> expired` state computed at read time.
async fn never_responded_request_survives_in_history_after_purge(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let b = account(s).await?;
    let p = person(s, &a).await?;
    let request = id();
    s.people_command(
        &a,
        &id(),
        &PeopleCommand::RequestLink {
            id: request.clone(),
            person_id: p,
            account_id: b,
            expected_version: 1,
        },
        NOW,
    )
    .await?;

    s.collect_expired(NOW + 604800).await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM people_requests WHERE id=$1")
            .bind(&request)
            .fetch_one(&s.pool)
            .await?,
        0,
        "the operational row is gone"
    );

    let page = s.sent_people_requests(&a, None, 200, NOW + 604800).await?;
    let item = page
        .items
        .iter()
        .find(|item| item.id == request)
        .expect("the durable history mirror survives the purge");
    assert_eq!(item.state, "expired");
    Ok(())
}
#[tokio::test]
async fn never_responded_request_history_survives_operational_purge() -> Result<()> {
    let (s, _d) = setup().await?;
    never_responded_request_survives_in_history_after_purge(&s).await
}
