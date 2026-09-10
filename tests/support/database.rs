use anyhow::Result;
use atlas_core::Store;

/// Each case owns a file or a unique schema. The PostgreSQL runner owns and
/// removes the disposable cluster, including schemas left by failed cases.
pub async fn database_url() -> Result<(tempfile::TempDir, String)> {
    let dir = tempfile::tempdir()?;
    let url = if let Ok(base) = std::env::var("ATLAS_TEST_POSTGRES_URL") {
        let admin = Store::connect(&base).await?;
        let schema = format!("case_{}", uuid::Uuid::new_v4().simple());
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin.pool)
            .await?;
        admin.pool.close().await;
        let mut url = url::Url::parse(&base)?;
        url.query_pairs_mut()
            .append_pair("options", &format!("--search_path={schema}"));
        url.to_string()
    } else {
        format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("db.sqlite").display()
        )
    };
    Ok((dir, url))
}
