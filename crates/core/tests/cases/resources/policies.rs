use anyhow::Result;
use atlas_core::{Change, Command, Store};

use crate::support::resource_commands::{account, create, fixture, id, share};

async fn policy_revisions(store: &Store) -> Result<()> {
    let owner = account(store).await?;
    let reader = account(store).await?;
    let person = create(store, &owner).await?;
    share(store, &owner, &person, &reader, 1).await?;
    // An offline content edit based on version 1 survives the unrelated policy edit.
    store
        .apply(
            &owner,
            &id(),
            &[Command::Edit {
                id: person.clone(),
                expected_version: 1,
                label: "Renamed".into(),
                value: String::new(),
            }],
        )
        .await?;
    let page = store.sync(&owner, "owner", None, 200, 1000).await?;
    let Change::Upsert { resource } = &page.batches[0].changes[0] else {
        panic!("missing person")
    };
    assert_eq!((resource.version, resource.policy_version), (2, Some(2)));
    let page = store.sync(&reader, "reader", None, 200, 1000).await?;
    assert!(!serde_json::to_string(&page)?.contains("policy_version"));
    Ok(())
}

#[tokio::test]
async fn policy_does_not_conflict_with_content() -> Result<()> {
    let (_dir, s) = fixture().await?;
    policy_revisions(&s).await
}

async fn privacy(store: &Store, case: u8) -> Result<()> {
    let owner = account(store).await?;
    let reader = account(store).await?;
    let outsider = account(store).await?;
    let person = create(store, &owner).await?;
    let shared = id();
    let private = id();
    store
        .apply(
            &owner,
            &id(),
            &[
                Command::CreateField {
                    initial_policy: None,
                    id: shared.clone(),
                    person_id: person.clone(),
                    label: "Shared".into(),
                    value: "Shared value".into(),
                },
                Command::CreateField {
                    initial_policy: None,
                    id: private.clone(),
                    person_id: person.clone(),
                    label: "Private label".into(),
                    value: "Private value".into(),
                },
            ],
        )
        .await?;
    if case == 1 {
        let error = store
            .apply(
                &owner,
                &id(),
                &[Command::Grant {
                    id: shared,
                    expected_version: 1,
                    account_id: reader,
                    edit: true,
                }],
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "identity_grant_required");
    } else {
        share(store, &owner, &person, &reader, 1).await?;
        share(store, &owner, &shared, &reader, 1).await?;
        if case == 2 {
            let page = store.sync(&reader, "phone", None, 200, 1000).await?;
            let text = serde_json::to_string(&page)?;
            assert!(text.contains("Shared value"));
            assert!(
                !text.contains("Private label")
                    && !text.contains("Private value")
                    && !text.contains(&private)
            );
        } else {
            let error = store
                .apply(
                    &reader,
                    &id(),
                    &[Command::Grant {
                        id: shared,
                        expected_version: 2,
                        account_id: outsider,
                        edit: true,
                    }],
                )
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), "forbidden");
        }
    }
    Ok(())
}

#[tokio::test]
async fn identity_is_required() -> Result<()> {
    let (_d, s) = fixture().await?;
    privacy(&s, 1).await
}

#[tokio::test]
async fn private_sibling_labels_stay_hidden() -> Result<()> {
    let (_d, s) = fixture().await?;
    privacy(&s, 2).await
}

#[tokio::test]
async fn editor_cannot_manage_sharing() -> Result<()> {
    let (_d, s) = fixture().await?;
    privacy(&s, 3).await
}
