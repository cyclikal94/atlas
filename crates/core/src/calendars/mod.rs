//! Calendar input is evidence about task timing, never authority to delete work.
pub mod ics;
mod service;
pub use service::{CalendarCommand, Refresh};
mod anchors;
pub use anchors::{Anchor, Offset, Reference};
mod reminders;
pub use reminders::{Delivery, DeliveryMode, Notification, ReminderCommand, ReminderRule};

// Ciphertext is randomised; its keyed fingerprint provides stable receipt input.
fn secret_identity(secret: &str) -> String {
    let parts = secret.splitn(3, ':').collect::<Vec<_>>();
    if parts.len() == 3 && parts[0] == "v1" {
        format!("v1:{}", parts[1])
    } else {
        secret.to_owned()
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
