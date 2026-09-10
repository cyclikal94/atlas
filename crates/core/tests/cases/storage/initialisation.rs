use anyhow::Result;
use atlas_core::Store;

async fn initialise_and_reopen(url: &str) -> Result<()> {
    let first = Store::connect(url).await?;
    let second = Store::connect(url).await?;
    let (a, b) = tokio::join!(first.migrate(), second.migrate());
    a?;
    b?;
    let id = uuid::Uuid::new_v4().to_string();
    first.add_account(&id, &id, "test-only").await?;
    first.pool.close().await;
    second.pool.close().await;
    let reopened = Store::connect(url).await?;
    reopened.migrate().await?;
    let found: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(&id)
        .fetch_one(&reopened.pool)
        .await?;
    assert_eq!(found, id);
    // An unsupported identity must fail without changing existing content.
    sqlx::query("UPDATE atlas_schema SET version=1")
        .execute(&reopened.pool)
        .await?;
    assert!(
        reopened
            .migrate()
            .await
            .unwrap_err()
            .to_string()
            .contains("explicit reset")
    );
    let found: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(&id)
        .fetch_one(&reopened.pool)
        .await?;
    assert_eq!(found, id);
    sqlx::query("UPDATE atlas_schema SET version=1000")
        .execute(&reopened.pool)
        .await?;
    reopened.pool.close().await;
    Ok(())
}

#[tokio::test]
async fn initialisation_reopen_and_incompatible_schema() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    initialise_and_reopen(&url).await
}

#[tokio::test]
async fn failed_initialisation_rolls_back() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    // A conflicting table fails after some baseline DDL has run.
    sqlx::query("CREATE TABLE resources (sentinel TEXT)")
        .execute(&store.pool)
        .await?;
    assert!(store.migrate().await.is_err());
    let tables: Vec<String> = sqlx::query_scalar(if url.starts_with("sqlite:") {
        "SELECT name FROM sqlite_schema WHERE type='table'"
    } else {
        "SELECT CAST(table_name AS TEXT) FROM information_schema.tables WHERE table_schema=current_schema()"
    })
    .fetch_all(&store.pool)
    .await?;
    assert_eq!(tables, ["resources"]);
    sqlx::query("DROP TABLE resources")
        .execute(&store.pool)
        .await?;
    store.migrate().await?;
    Ok(())
}
