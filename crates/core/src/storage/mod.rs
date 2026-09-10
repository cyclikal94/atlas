use crate::Store;
use anyhow::{Result, ensure};
use sqlx::{Acquire, Any, Connection, Transaction, any::AnyPoolOptions};

impl Store {
    /// Reset delivery and login state on a restored copy, with all servers stopped.
    /// Domain data and operation receipts retain their original identities.
    pub async fn prepare_restored_database(&self) -> Result<()> {
        let mut tx = self.begin_serial().await?;
        for statement in [
            "DELETE FROM sync_cursors",
            "DELETE FROM sync_snapshots",
            "DELETE FROM sync_devices",
            "DELETE FROM sessions",
            "DELETE FROM oidc_flows",
            "DELETE FROM native_handoffs",
        ] {
            sqlx::query(statement).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE accounts SET access_epoch=access_epoch+1")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE account_invitations SET revoked=1")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE calendar_sources SET generation=generation+1,lease_until=0")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE reminder_deliveries SET lease_token=NULL,lease_until=0")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Initialise the current schema atomically, or validate an existing baseline.
    /// Unreleased historical schemas require an explicit operator-managed reset.
    pub async fn migrate(&self) -> Result<()> {
        let mut connection = self.pool.acquire().await?;
        let mut tx = if self.sqlite {
            connection.begin_with("BEGIN IMMEDIATE").await?
        } else {
            connection.begin().await?
        };
        if !self.sqlite {
            sqlx::query("SELECT pg_advisory_xact_lock(728194620)")
                .execute(&mut *tx)
                .await?;
        }
        let exists: i64 = sqlx::query_scalar(if self.sqlite {
            "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name='atlas_schema'"
        } else {
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema=current_schema() AND table_name='atlas_schema'"
        }).fetch_one(&mut *tx).await?;
        if exists == 0 {
            sqlx::raw_sql(if self.sqlite {
                include_str!("schema/sqlite.sql")
            } else {
                include_str!("schema/postgres.sql")
            })
            .execute(&mut *tx)
            .await?;
        } else {
            let versions: Vec<i64> =
                sqlx::query_scalar("SELECT version FROM atlas_schema ORDER BY version")
                    .fetch_all(&mut *tx)
                    .await?;
            ensure!(
                versions == [1000],
                "unsupported_schema: this pre-release database requires an explicit reset"
            );
        }
        tx.commit().await?;
        Ok(())
    }
}

impl Store {
    pub async fn connect(url: &str) -> Result<Self> {
        sqlx::any::install_default_drivers();
        let sqlite = url.starts_with("sqlite:");
        let pool = AnyPoolOptions::new()
            .max_connections(10)
            .after_connect(move |c, _| {
                Box::pin(async move {
                    if sqlite {
                        sqlx::raw_sql("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=10000;")
                            .execute(c)
                            .await?;
                    }
                    Ok(())
                })
            })
            .connect(url)
            .await?;
        if sqlite {
            sqlx::raw_sql("PRAGMA journal_mode=WAL;")
                .execute(&pool)
                .await?;
        }
        Ok(Self {
            pool,
            sqlite,
            retention_seconds: 90 * 86400,
        })
    }

    pub fn with_retention_days(mut self, days: i64) -> Result<Self> {
        ensure!(
            (1..=3650).contains(&days),
            "invalid retention days: expected 1..3650"
        );
        self.retention_seconds = days * 86400;
        Ok(self)
    }

    pub async fn begin_serial(&self) -> Result<Transaction<'static, Any>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE sync_clock SET revision=revision WHERE id=1")
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }
}
