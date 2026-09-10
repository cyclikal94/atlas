use anyhow::Result;
use atlas_core::Store;
use atlas_server::{App, hash_password};
use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;
fn id() -> String {
    Uuid::new_v4().to_string()
}
use crate::support::http::command_request as call;
#[tokio::test]
async fn dependencies_timers_and_presets() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let actor = id();
    store
        .add_account(
            &actor,
            "workflow-user",
            &hash_password("workflow-password-12345".into()).await?,
        )
        .await?;
    let app = App::new(store).await?.router();
    let (status, login) = call(
        &app,
        "sessions",
        None,
        None,
        Some(json!({"username":"workflow-user","password":"workflow-password-12345","device_id":"phone"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = login["access_token"].as_str().unwrap();
    let tasks = [id(), id(), id()];
    let occurrences = tasks
        .iter()
        .map(|t| atlas_core::tasks::occurrence_id(t, "once"))
        .collect::<Result<Vec<_>>>()?;
    for (i, task) in tasks.iter().enumerate() {
        let goal = if i == 2 {
            json!({"kind":"numeric","minimum":"60","maximum":null,"unit":"seconds"})
        } else {
            json!({"kind":"checkbox"})
        };
        let body = json!({"command":{"kind":"create_task","id":task,"execution_id":id(),"title":"Example","definition":{"schedule":{"start_date":null,"time":null,"timezone":"Europe/Vienna","repeat":null},"goal":goal,"carry":"retain_one","participation":"personal","open_days_before":0,"close_days_after":1}}});
        let result = call(&app, "task-commands", Some(token), Some(&id()), Some(body)).await;
        assert_eq!(result.0, StatusCode::OK, "{:?}", result.1);
    }
    let dependency_path = format!("occurrences/{}/dependencies", occurrences[1]);
    assert_eq!(
        call(&app, &dependency_path, None, None, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let result=call(&app,"task-commands",Some(token),Some(&id()),Some(json!({"command":{"kind":"set_dependencies","occurrence_id":occurrences[1],"expected_version":1,"prerequisites":[occurrences[0]],"strict":false}}))).await;
    assert_eq!(result.0, StatusCode::OK, "{:?}", result.1);
    let preview = call(&app, &dependency_path, Some(token), None, None).await;
    assert_eq!(preview.0, StatusCode::OK);
    assert_eq!(preview.1["items"].as_array().unwrap().len(), 2);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let body = json!({"command":{"kind":"complete_dependencies","occurrence_id":occurrences[1],"preview_token":preview.1["token"],"mode":"complete_prerequisites","happened_at":now}});
    let op = id();
    let result = call(
        &app,
        "task-commands",
        Some(token),
        Some(&op),
        Some(body.clone()),
    )
    .await;
    assert_eq!(result.0, StatusCode::OK, "{:?}", result.1);
    assert_eq!(
        result,
        call(&app, "task-commands", Some(token), Some(&op), Some(body)).await
    );
    let session = id();
    for command in [
        json!({"kind":"start_timer","occurrence_id":occurrences[2],"session_id":session,"started_at":now-120}),
        json!({"kind":"stop_timer","occurrence_id":occurrences[2],"session_id":session,"expected_version":1,"stopped_at":now}),
    ] {
        let result = call(
            &app,
            "task-commands",
            Some(token),
            Some(&id()),
            Some(json!({"command":command})),
        )
        .await;
        assert_eq!(result.0, StatusCode::OK, "{:?}", result.1);
    }
    let timers = call(
        &app,
        &format!("occurrences/{}/timers?limit=10", occurrences[2]),
        Some(token),
        None,
        None,
    )
    .await;
    assert_eq!(timers.0, StatusCode::OK);
    assert_eq!(timers.1["items"][0]["stopped_at"], now);
    let presets = call(
        &app,
        "task-presets?start_date=2026-09-09&timezone=Europe%2FVienna",
        Some(token),
        None,
        None,
    )
    .await;
    assert_eq!(presets.0, StatusCode::OK);
    assert_eq!(presets.1["items"].as_array().unwrap().len(), 6);
    for preset in presets.1["items"].as_array().unwrap() {
        let definition: atlas_core::tasks::Definition =
            serde_json::from_value(preset["definition"].clone())?;
        definition.validate()?;
    }
    Ok(())
}
