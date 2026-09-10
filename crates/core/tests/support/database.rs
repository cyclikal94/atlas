use anyhow::Result;
use atlas_core::Store;
#[path = "../../../../tests/support/database.rs"]
mod backend;
pub use backend::database_url;
pub fn postgres() -> bool {
    std::env::var_os("ATLAS_TEST_POSTGRES_URL").is_some()
}
pub async fn fixture() -> Result<(tempfile::TempDir, Store)> {
    let (dir, url) = database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    Ok((dir, store))
}

pub async fn setup() -> Result<(Store, tempfile::TempDir)> {
    let (dir, store) = fixture().await?;
    Ok((store, dir))
}
