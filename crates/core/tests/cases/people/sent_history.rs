//! BE-Q16: the durable, sender-keyed, all-states, paginated `sent_people_requests` read.
use anyhow::Result;
use atlas_core::{Store, people::PeopleCommand};
use std::collections::BTreeSet;

use crate::support::task_fixtures::{account, id, person, setup};

const NOW: i64 = 1788868800;

/// A `RequestLink` naming `recipient` as recipient, owned by `sender`. The proposal kind does
/// not matter to the sent-history read; `RequestLink` needs the least setup.
async fn linked_request(s: &Store, sender: &str, recipient: &str) -> Result<String> {
    let p = person(s, sender).await?;
    let request = id();
    s.people_command(
        sender,
        &id(),
        &PeopleCommand::RequestLink {
            id: request.clone(),
            person_id: p,
            account_id: recipient.into(),
            expected_version: 1,
        },
        NOW,
    )
    .await?;
    Ok(request)
}

async fn all_states_scenario(s: &Store) -> Result<()> {
    let sender = account(s).await?;
    let recipient = account(s).await?;

    let pending = linked_request(s, &sender, &recipient).await?;
    let accepted = linked_request(s, &sender, &recipient).await?;
    s.people_command(
        &recipient,
        &id(),
        &PeopleCommand::RespondRequest {
            id: accepted.clone(),
            accept: true,
            recipient_preview_token: None,
        },
        NOW,
    )
    .await?;
    let declined = linked_request(s, &sender, &recipient).await?;
    s.people_command(
        &recipient,
        &id(),
        &PeopleCommand::RespondRequest {
            id: declined.clone(),
            accept: false,
            recipient_preview_token: None,
        },
        NOW,
    )
    .await?;
    let cancelled = linked_request(s, &sender, &recipient).await?;
    s.people_command(
        &sender,
        &id(),
        &PeopleCommand::CancelRequest {
            id: cancelled.clone(),
        },
        NOW,
    )
    .await?;
    let expiring = linked_request(s, &sender, &recipient).await?;

    // Before the expiry horizon: the never-responded request is still "pending", and a request
    // already resolved by time it went stale keeps its true terminal state, not "expired".
    let page = s.sent_people_requests(&sender, None, 200, NOW).await?;
    let states: std::collections::BTreeMap<String, String> = page
        .items
        .iter()
        .map(|item| (item.id.clone(), item.state.clone()))
        .collect();
    assert_eq!(states[&pending], "pending");
    assert_eq!(states[&accepted], "accepted");
    assert_eq!(states[&declined], "declined");
    assert_eq!(states[&cancelled], "cancelled");
    assert_eq!(states[&expiring], "pending");
    assert!(page.next_after.is_none());

    // Past the expiry horizon: only the never-responded request computes as "expired" — the
    // "pending && expires_at<=now" rule must not misfire on already-terminal rows.
    let later = s
        .sent_people_requests(&sender, None, 200, NOW + 604801)
        .await?;
    let states: std::collections::BTreeMap<String, String> = later
        .items
        .iter()
        .map(|item| (item.id.clone(), item.state.clone()))
        .collect();
    assert_eq!(states[&pending], "expired");
    assert_eq!(states[&accepted], "accepted");
    assert_eq!(states[&declined], "declined");
    assert_eq!(states[&cancelled], "cancelled");
    assert_eq!(states[&expiring], "expired");

    // A sender with no requests sees an empty, terminated page.
    let stranger = account(s).await?;
    let empty = s.sent_people_requests(&stranger, None, 200, NOW).await?;
    assert!(empty.items.is_empty());
    assert!(empty.next_after.is_none());

    // Invalid inputs are rejected.
    assert!(s.sent_people_requests(&sender, None, 0, NOW).await.is_err());
    assert!(
        s.sent_people_requests(&sender, None, 201, NOW)
            .await
            .is_err()
    );
    assert!(
        s.sent_people_requests(&sender, Some("not-a-uuid"), 50, NOW)
            .await
            .is_err()
    );
    Ok(())
}

async fn pagination_scenario(s: &Store) -> Result<()> {
    let sender = account(s).await?;
    let recipient = account(s).await?;
    let mut created = BTreeSet::new();
    for _ in 0..5 {
        created.insert(linked_request(s, &sender, &recipient).await?);
    }

    let mut seen = BTreeSet::new();
    let mut after: Option<String> = None;
    loop {
        let page = s
            .sent_people_requests(&sender, after.as_deref(), 2, NOW)
            .await?;
        let exact_page = page.items.len() == 2;
        assert_eq!(
            page.next_after.is_some(),
            exact_page && seen.len() + page.items.len() < created.len(),
            "next_after is set exactly when a full page leaves more rows unread"
        );
        for item in &page.items {
            seen.insert(item.id.clone());
        }
        after = page.next_after;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(seen, created);
    Ok(())
}

/// Two concurrent `RespondRequest` calls (accept and decline) against the same pending row:
/// exactly one commits, and the operational and durable history rows end up consistent with
/// each other, not just internally consistent on one side.
async fn concurrent_responses_scenario(s: &Store) -> Result<()> {
    let sender = account(s).await?;
    let recipient = account(s).await?;
    let request = linked_request(s, &sender, &recipient).await?;

    let accept_op = id();
    let accept_command = PeopleCommand::RespondRequest {
        id: request.clone(),
        accept: true,
        recipient_preview_token: None,
    };
    let decline_op = id();
    let decline_command = PeopleCommand::RespondRequest {
        id: request.clone(),
        accept: false,
        recipient_preview_token: None,
    };
    let accept = s.people_command(&recipient, &accept_op, &accept_command, NOW);
    let decline = s.people_command(&recipient, &decline_op, &decline_command, NOW);
    let (accept, decline) = tokio::join!(accept, decline);
    assert_ne!(accept.is_ok(), decline.is_ok());

    let operational: String = sqlx::query_scalar("SELECT state FROM people_requests WHERE id=$1")
        .bind(&request)
        .fetch_one(&s.pool)
        .await?;
    let historical: String =
        sqlx::query_scalar("SELECT state FROM people_request_history WHERE id=$1")
            .bind(&request)
            .fetch_one(&s.pool)
            .await?;
    assert_eq!(operational, historical);
    assert!(operational == "accepted" || operational == "declined");
    Ok(())
}

#[tokio::test]
async fn sent_people_requests_reports_all_states_including_computed_expiry() -> Result<()> {
    let (s, _dir) = setup().await?;
    all_states_scenario(&s).await
}

#[tokio::test]
async fn sent_people_requests_pages_exhaustively() -> Result<()> {
    let (s, _dir) = setup().await?;
    pagination_scenario(&s).await
}

#[tokio::test]
async fn concurrent_respond_request_calls_keep_history_consistent() -> Result<()> {
    let (s, _dir) = setup().await?;
    concurrent_responses_scenario(&s).await
}
