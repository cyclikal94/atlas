use super::*;
use crate::error::ErrorCode;
#[derive(Serialize)]
pub struct Device {
    pub id: String,
    pub last_synced_at: Option<i64>,
    pub active_sessions: i64,
}
impl Store {
    pub async fn devices(&self, actor: &str, now: i64) -> Result<Vec<Device>> {
        let rows=sqlx::query("SELECT d.device_id,s.last_seen,(SELECT COUNT(*) FROM sessions x WHERE x.account_id=$1 AND x.device_id=d.device_id AND x.expires_at>$2) FROM (SELECT device_id FROM sync_devices WHERE account_id=$1 UNION SELECT device_id FROM sessions WHERE account_id=$1 AND expires_at>$2) d LEFT JOIN sync_devices s ON s.account_id=$1 AND s.device_id=d.device_id ORDER BY d.device_id").bind(actor).bind(now).fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .map(|r| Device {
                id: r.get(0),
                last_synced_at: r.get(1),
                active_sessions: r.get(2),
            })
            .collect())
    }
    /// Revocation preserves account-level operation receipts and all domain data.
    pub async fn forget_device(&self, actor: &str, device: &str) -> Result<()> {
        ensure!(
            !device.is_empty() && device.len() <= 100,
            ErrorCode::InvalidValue
        );
        let mut tx = self.begin_serial().await?;
        Self::epoch(&mut tx, actor).await?;
        sqlx::query("DELETE FROM sync_cursors WHERE account_id=$1 AND device_id=$2")
            .bind(actor)
            .bind(device)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sync_devices WHERE account_id=$1 AND device_id=$2")
            .bind(actor)
            .bind(device)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sessions WHERE account_id=$1 AND device_id=$2")
            .bind(actor)
            .bind(device)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM native_handoffs WHERE account_id=$1 AND device_id=$2")
            .bind(actor)
            .bind(device)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE notification_subscriptions SET active=0,secret='',version=version+1 WHERE account_id=$1 AND device_id=$2 AND active=1").bind(actor).bind(device).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}
