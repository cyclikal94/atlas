use crate::error::ErrorCode;
use crate::{Store, identifier};
use anyhow::{Result, ensure};
use sqlx::{Any, Transaction};
impl Store {
    /// Operator-only bootstrap, deliberately not an unauthenticated HTTP route.
    pub async fn add_account(&self, id: &str, username: &str, password_hash: &str) -> Result<()> {
        let mut tx = self.begin_serial().await?;
        Self::add_account_in(&mut tx, id, username, password_hash).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Account creation inside an already serialised, authorised transaction.
    pub async fn add_account_in(
        tx: &mut Transaction<'_, Any>,
        id: &str,
        username: &str,
        password_hash: &str,
    ) -> Result<()> {
        identifier(id)?;
        ensure!(
            !username.is_empty()
                && username.len() <= 100
                && username
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_-.".contains(&c)),
            ErrorCode::InvalidValue
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
            .fetch_one(&mut **tx)
            .await?;
        ensure!(count < 100, ErrorCode::SliceCapacity);
        sqlx::query("INSERT INTO accounts(id,username,password_hash) VALUES ($1,$2,$3)")
            .bind(id)
            .bind(username)
            .bind(password_hash)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
}
