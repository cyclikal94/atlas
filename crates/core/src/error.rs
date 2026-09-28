//! Stable application failures. Storage/provider details stay outside the wire contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    AccessChanged,
    BatchTooLarge,
    CalendarLimit,
    Conflict,
    ConnectionUnavailable,
    CredentialMismatch,
    DefaultsChanged,
    DeliveryFailed,
    DeviceCapacity,
    ExpansionLimit,
    FetchFailed,
    Forbidden,
    IdentityGrantRequired,
    IntegrationUnconfigured,
    InternalError,
    InvalidEndpoint,
    InvalidIcs,
    InvalidSecret,
    InvalidSubscription,
    InvalidValue,
    InvitationExpired,
    LastManager,
    MalformedRequest,
    MaterialisationRequired,
    NotFound,
    NotReady,
    OidcUnavailable,
    OperationConflict,
    OutboundDenied,
    PasswordHashFailed,
    PasswordLength,
    RateLimited,
    RefreshInProgress,
    ReminderTimeRequired,
    ResyncRequired,
    SecretEncryptionFailed,
    SliceCapacity,
    SourceConnectionRequired,
    StaleDelivery,
    StaleRefresh,
    TemporarilyUnavailable,
    Unauthenticated,
    UnsupportedCalendarRule,
    UnsupportedCalendarTimezone,
    UnsupportedReceipt,
}
impl ErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AccessChanged => "access_changed",
            Self::BatchTooLarge => "batch_too_large",
            Self::CalendarLimit => "calendar_limit",
            Self::Conflict => "conflict",
            Self::ConnectionUnavailable => "connection_unavailable",
            Self::CredentialMismatch => "credential_mismatch",
            Self::DefaultsChanged => "defaults_changed",
            Self::DeliveryFailed => "delivery_failed",
            Self::DeviceCapacity => "device_capacity",
            Self::ExpansionLimit => "expansion_limit",
            Self::FetchFailed => "fetch_failed",
            Self::Forbidden => "forbidden",
            Self::IdentityGrantRequired => "identity_grant_required",
            Self::IntegrationUnconfigured => "integration_unconfigured",
            Self::InternalError => "internal_error",
            Self::InvalidEndpoint => "invalid_endpoint",
            Self::InvalidIcs => "invalid_ics",
            Self::InvalidSecret => "invalid_secret",
            Self::InvalidSubscription => "invalid_subscription",
            Self::InvalidValue => "invalid_value",
            Self::InvitationExpired => "invitation_expired",
            Self::LastManager => "last_manager",
            Self::MalformedRequest => "malformed_request",
            Self::MaterialisationRequired => "materialisation_required",
            Self::NotFound => "not_found",
            Self::NotReady => "not_ready",
            Self::OidcUnavailable => "oidc_unavailable",
            Self::OperationConflict => "operation_conflict",
            Self::OutboundDenied => "outbound_denied",
            Self::PasswordHashFailed => "password_hash_failed",
            Self::PasswordLength => "password_length",
            Self::RateLimited => "rate_limited",
            Self::RefreshInProgress => "refresh_in_progress",
            Self::ReminderTimeRequired => "reminder_time_required",
            Self::ResyncRequired => "resync_required",
            Self::SecretEncryptionFailed => "secret_encryption_failed",
            Self::SliceCapacity => "slice_capacity",
            Self::SourceConnectionRequired => "source_connection_required",
            Self::StaleDelivery => "stale_delivery",
            Self::StaleRefresh => "stale_refresh",
            Self::TemporarilyUnavailable => "temporarily_unavailable",
            Self::Unauthenticated => "unauthenticated",
            Self::UnsupportedCalendarRule => "unsupported_calendar_rule",
            Self::UnsupportedCalendarTimezone => "unsupported_calendar_timezone",
            Self::UnsupportedReceipt => "unsupported_receipt",
        }
    }
}
impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
impl std::error::Error for ErrorCode {}
/// Distinguishes B8 causes 4 and 5, which both raise `ErrorCode::StaleRefresh`
/// (`409 stale_refresh`): a generation change proves something else touched
/// this source since the attempt began — a newer refresh or a concurrent
/// settings edit (cause 4) — while an unchanged generation with an expired
/// lease means only this attempt's own lease timed out (cause 5). The two
/// need different client handling despite sharing a wire code, so the HTTP
/// layer reports this as an additional `reason` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaleRefreshReason {
    GenerationChanged,
    LeaseExpired,
}
impl StaleRefreshReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GenerationChanged => "generation_changed",
            Self::LeaseExpired => "lease_expired",
        }
    }
}
impl std::fmt::Display for StaleRefreshReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
impl std::error::Error for StaleRefreshReason {}
