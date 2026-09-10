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
                accept: true
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
