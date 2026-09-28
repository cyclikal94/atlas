use crate::Store;
use anyhow::{Result, anyhow, ensure};
use sqlx::{Acquire, Any, Connection, Transaction, any::AnyPoolOptions};

/// Version recorded by a freshly initialised database; the last step of `UPGRADES`.
const SCHEMA_VERSION: i64 = 1004;

/// One additive, in-place step. Steps run inside `migrate()`'s serialising transaction, in
/// order, and each must move the recorded version forward by exactly its `to`.
struct Upgrade {
    from: i64,
    to: i64,
    sqlite: &'static str,
    postgres: &'static str,
}
const UPGRADES: &[Upgrade] = &[
    Upgrade {
        from: 1000,
        to: 1001,
        sqlite: include_str!("schema/upgrade_1001_sqlite.sql"),
        postgres: include_str!("schema/upgrade_1001_postgres.sql"),
    },
    Upgrade {
        from: 1001,
        to: 1002,
        sqlite: include_str!("schema/upgrade_1002_sqlite.sql"),
        postgres: include_str!("schema/upgrade_1002_postgres.sql"),
    },
    Upgrade {
        from: 1002,
        to: 1003,
        sqlite: include_str!("schema/upgrade_1003_sqlite.sql"),
        postgres: include_str!("schema/upgrade_1003_postgres.sql"),
    },
    Upgrade {
        from: 1003,
        to: 1004,
        sqlite: include_str!("schema/upgrade_1004_sqlite.sql"),
        postgres: include_str!("schema/upgrade_1004_postgres.sql"),
    },
];

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
            "DELETE FROM activation_grants",
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
                versions.len() == 1,
                "unsupported_schema: this pre-release database requires an explicit reset"
            );
            // An older baseline walks the additive steps; a newer or unknown one has no step
            // and keeps the operator-managed reset error. A downgrade needs a restore.
            let mut current = versions[0];
            while current != SCHEMA_VERSION {
                let step = UPGRADES.iter().find(|u| u.from == current).ok_or_else(|| {
                    anyhow!(
                        "unsupported_schema: this pre-release database requires an explicit reset"
                    )
                })?;
                sqlx::raw_sql(if self.sqlite {
                    step.sqlite
                } else {
                    step.postgres
                })
                .execute(&mut *tx)
                .await?;
                sqlx::query("UPDATE atlas_schema SET version=$1 WHERE version=$2")
                    .bind(step.to)
                    .bind(step.from)
                    .execute(&mut *tx)
                    .await?;
                current = step.to;
            }
        }
        tx.commit().await?;
        Ok(())
    }
}

impl Store {
    pub async fn connect(url: &str) -> Result<Self> {
        Self::connect_with(url, 10_000).await
    }

    /// As `connect`, with SQLite's busy timeout chosen by the test that induces `SQLITE_BUSY`.
    #[cfg(feature = "test-hooks")]
    pub async fn connect_with_busy_timeout(url: &str, milliseconds: u32) -> Result<Self> {
        Self::connect_with(url, milliseconds).await
    }

    async fn connect_with(url: &str, busy_timeout: u32) -> Result<Self> {
        sqlx::any::install_default_drivers();
        let sqlite = url.starts_with("sqlite:");
        let pool = AnyPoolOptions::new()
            .max_connections(10)
            .after_connect(move |c, _| {
                Box::pin(async move {
                    if sqlite {
                        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                            "PRAGMA foreign_keys=ON; PRAGMA busy_timeout={busy_timeout};"
                        )))
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
            #[cfg(feature = "test-hooks")]
            hooks: Default::default(),
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

    /// `begin_serial` for the retirement, which a test may ask to run at REPEATABLE READ.
    pub(crate) async fn begin_retirement(&self) -> Result<Transaction<'static, Any>> {
        #[cfg(feature = "test-hooks")]
        if !self.sqlite && self.hooks.raised_isolation() {
            let mut tx = self.pool.begin().await?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE sync_clock SET revision=revision WHERE id=1")
                .execute(&mut *tx)
                .await?;
            return Ok(tx);
        }
        self.begin_serial().await
    }

    /// Whether member rows are read `FOR UPDATE`: always on PostgreSQL in a release build.
    pub(crate) fn locks_rows(&self) -> bool {
        #[cfg(feature = "test-hooks")]
        if !self.hooks.locking() {
            return false;
        }
        !self.sqlite
    }

    pub async fn begin_serial(&self) -> Result<Transaction<'static, Any>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE sync_clock SET revision=revision WHERE id=1")
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chain must be contiguous from the first upgradable baseline to the current version,
    /// or `migrate()` could loop or skip a step.
    #[test]
    fn upgrade_chain_is_contiguous_and_ends_at_the_current_version() {
        let mut version = 1000;
        for step in UPGRADES {
            assert_eq!(step.from, version);
            assert!(step.to > step.from);
            version = step.to;
        }
        assert_eq!(version, SCHEMA_VERSION);
        for baseline in [
            include_str!("schema/sqlite.sql"),
            include_str!("schema/postgres.sql"),
        ] {
            assert!(baseline.contains(&format!(
                "INSERT INTO atlas_schema(version) VALUES({SCHEMA_VERSION});"
            )));
        }
    }

    /// `scripts/backup.py` refuses a bundle whose schema it does not list, so every version the
    /// server can hold or walk (the first upgradable baseline to the current one) must be listed.
    /// A schema bump that forgets the tool fails here, in the ordinary test job, instead of at
    /// the moment an operator needs a backup (the 1003 defect).
    #[test]
    fn recovery_tool_supports_every_schema_the_server_can_migrate() {
        let source = include_str!("../../../../scripts/backup.py");
        let line = source
            .lines()
            .find(|l| l.starts_with("SUPPORTED_SCHEMAS = ("))
            .expect("backup.py declares SUPPORTED_SCHEMAS");
        let listed: Vec<i64> = line
            .trim_start_matches("SUPPORTED_SCHEMAS = (")
            .trim_end_matches(')')
            .split(',')
            .filter(|v| !v.trim().is_empty())
            .map(|v| v.trim().parse().expect("a numeric schema version"))
            .collect();
        let first = UPGRADES.first().expect("at least one upgrade").from;
        let expected: Vec<i64> = (first..=SCHEMA_VERSION).collect();
        assert_eq!(
            listed, expected,
            "scripts/backup.py SUPPORTED_SCHEMAS must be {first}..={SCHEMA_VERSION}"
        );
    }
}
