use crate::support::http::session_request as call;
use atlas_core::{Command, Store, households::ManagementCommand};
use atlas_server::{App, hash_password, now, operator_invitation};
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;
fn registration(token: &str, username: &str) -> Value {
    json!({"token":token,"username":username,"password":"onboarding-password-123","device_id":"phone"})
}
async fn scenario(store: Store) -> anyhow::Result<()> {
    store.migrate().await?;
    let owner = Uuid::new_v4().to_string();
    let household = Uuid::new_v4().to_string();
    let person = Uuid::new_v4().to_string();
    let field = Uuid::new_v4().to_string();
    store
        .add_account(
            &owner,
            "invite-owner",
            &hash_password("onboarding-password-123".into()).await?,
        )
        .await?;
    store
        .management(
            &owner,
            &Uuid::new_v4().to_string(),
            &[ManagementCommand::CreateHousehold {
                id: household.clone(),
                name: "Home".into(),
            }],
            now(),
        )
        .await?;
    store
        .apply(
            &owner,
            &Uuid::new_v4().to_string(),
            &[
                Command::CreatePerson {
                    id: person.clone(),
                    name: "Shared friend".into(),
                    initial_policy: None,
                },
                Command::CreateField {
                    id: field.clone(),
                    person_id: person.clone(),
                    label: "Private notes".into(),
                    value: "Private content".into(),
                    initial_policy: None,
                },
            ],
        )
        .await?;
    let app = App::new(store.clone()).await?.router();
    let (_, session) = call(
        &app,
        "POST",
        "sessions",
        None,
        json!({"username":"invite-owner","password":"onboarding-password-123","device_id":"phone"}),
    )
    .await;
    let owner_token = session["access_token"].as_str().unwrap();
    let standalone = operator_invitation(&store).await?;
    let (status, outsider) = call(
        &app,
        "POST",
        "registration",
        None,
        registration(&standalone.token, "invite-outsider"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let outsider_token = outsider["access_token"].as_str().unwrap();
    assert_eq!(
        call(&app, "GET", "households", Some(outsider_token), Value::Null)
            .await
            .1,
        json!([])
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "account-invitations",
            Some(outsider_token),
            json!({"household_id":household})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, invitation) = call(
        &app,
        "POST",
        "account-invitations",
        Some(owner_token),
        json!({"household_id":household}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = invitation["token"].as_str().unwrap();
    let (status, preview) = call(
        &app,
        "POST",
        "registration/preview",
        None,
        json!({"token":token}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(preview["household"]["id"], household);
    assert_eq!(preview["household"]["name"], "Home");
    let (_, list) = call(
        &app,
        "GET",
        "account-invitations",
        Some(owner_token),
        Value::Null,
    )
    .await;
    assert!(!list.to_string().contains(token));
    assert!(list["invitations"][0].get("token").is_none());
    assert_eq!(
        call(
            &app,
            "DELETE",
            &format!("account-invitations/{}", invitation["id"].as_str().unwrap()),
            Some(outsider_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // A duplicate username must roll back the account and preserve the invitation.
    assert_eq!(
        call(
            &app,
            "POST",
            "registration",
            None,
            registration(token, "invite-owner")
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (first, second) = tokio::join!(
        call(
            &app,
            "POST",
            "registration",
            None,
            registration(token, "invite-one")
        ),
        call(
            &app,
            "POST",
            "registration",
            None,
            registration(token, "invite-two")
        )
    );
    assert!([first.0, second.0].contains(&StatusCode::OK));
    assert!([first.0, second.0].contains(&StatusCode::UNAUTHORIZED));
    let member = if first.0 == StatusCode::OK {
        first.1
    } else {
        second.1
    };
    let member_token = member["access_token"].as_str().unwrap();
    let member_id = member["account_id"].as_str().unwrap();
    assert_eq!(
        call(
            &app,
            "POST",
            "registration",
            None,
            registration(token, "invite-replay")
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (_, page) = call(&app, "GET", "sync", Some(member_token), Value::Null).await;
    let encoded = page.to_string();
    assert!(encoded.contains(&person));
    assert!(!encoded.contains(&field));
    assert!(!encoded.contains("Private"));
    assert_eq!(
        call(&app, "GET", "defaults", Some(member_token), Value::Null)
            .await
            .1["primary_household_id"],
        household
    );
    let (_, revoked) = call(
        &app,
        "POST",
        "account-invitations",
        Some(owner_token),
        json!({"household_id":household}),
    )
    .await;
    assert_eq!(
        call(
            &app,
            "DELETE",
            &format!("account-invitations/{}", revoked["id"].as_str().unwrap()),
            Some(owner_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "registration",
            None,
            registration(revoked["token"].as_str().unwrap(), "invite-revoked")
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let expired = operator_invitation(&store).await?;
    sqlx::query("UPDATE account_invitations SET expires_at=0 WHERE id=$1")
        .bind(expired.id)
        .execute(&store.pool)
        .await?;
    assert_eq!(
        call(
            &app,
            "POST",
            "registration",
            None,
            registration(&expired.token, "invite-expired")
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (_, pending) = call(
        &app,
        "POST",
        "account-invitations",
        Some(owner_token),
        json!({"household_id":household}),
    )
    .await;
    let version: i64 = sqlx::query_scalar("SELECT version FROM households WHERE id=$1")
        .bind(&household)
        .fetch_one(&store.pool)
        .await?;
    store
        .management(
            &owner,
            &Uuid::new_v4().to_string(),
            &[ManagementCommand::SetHouseholdRole {
                household_id: household.clone(),
                account_id: member_id.into(),
                expected_version: version,
                manager: true,
            }],
            now(),
        )
        .await?;
    store
        .management(
            &owner,
            &Uuid::new_v4().to_string(),
            &[ManagementCommand::SetHouseholdRole {
                household_id: household.clone(),
                account_id: owner.clone(),
                expected_version: version + 1,
                manager: false,
            }],
            now(),
        )
        .await?;
    store
        .management(
            member_id,
            &Uuid::new_v4().to_string(),
            &[ManagementCommand::SetHouseholdRole {
                household_id: household.clone(),
                account_id: owner.clone(),
                expected_version: version + 2,
                manager: true,
            }],
            now(),
        )
        .await?;
    assert_eq!(
        call(
            &app,
            "POST",
            "registration",
            None,
            registration(pending["token"].as_str().unwrap(), "invite-demoted")
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED,
        "restored manager rights must not reactivate a revoked invitation"
    );
    Ok(())
}
#[tokio::test]
async fn invited_registration() -> anyhow::Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    scenario(Store::connect(&url).await?).await
}
