use crate::REQUEST_ID;
use atlas_core::error::ErrorCode;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use uuid::Uuid;
pub struct ApiError(pub(crate) anyhow::Error);
impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self(error.into())
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        Self(value)
    }
}
impl From<sqlx::Error> for ApiError {
    fn from(value: sqlx::Error) -> Self {
        Self(value.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let application = self.0.downcast_ref::<ErrorCode>().copied();
        let unique = self
            .0
            .downcast_ref::<sqlx::Error>()
            .and_then(|e| e.as_database_error())
            .is_some_and(|e| e.is_unique_violation());
        let busy = self
            .0
            .downcast_ref::<sqlx::Error>()
            .and_then(|e| e.as_database_error())
            .and_then(|e| e.code())
            .is_some_and(|c| matches!(c.as_ref(), "5" | "517" | "40001" | "40P01"));
        let (status, code) = match application {
            Some(ErrorCode::OidcUnavailable) => (StatusCode::BAD_GATEWAY, "oidc_unavailable"),
            Some(ErrorCode::Unauthenticated) => (StatusCode::UNAUTHORIZED, "unauthenticated"),
            Some(ErrorCode::Forbidden) => (StatusCode::FORBIDDEN, "forbidden"),
            Some(ErrorCode::NotFound) => (StatusCode::NOT_FOUND, "not_found"),
            Some(ErrorCode::AccessChanged) => (StatusCode::CONFLICT, "access_changed"),
            Some(ErrorCode::ResyncRequired) => (StatusCode::GONE, "resync_required"),
            Some(ErrorCode::OperationConflict) => (StatusCode::CONFLICT, "operation_conflict"),
            Some(ErrorCode::Conflict) => (StatusCode::CONFLICT, "conflict"),
            Some(ErrorCode::InvalidIcs) => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_ics"),
            Some(ErrorCode::UnsupportedCalendarTimezone) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported_calendar_timezone",
            ),
            Some(ErrorCode::UnsupportedCalendarRule) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported_calendar_rule",
            ),
            Some(ErrorCode::CalendarLimit) => (StatusCode::UNPROCESSABLE_ENTITY, "calendar_limit"),
            Some(ErrorCode::InvalidEndpoint) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "invalid_endpoint")
            }
            Some(ErrorCode::OutboundDenied) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "outbound_denied")
            }
            Some(ErrorCode::InvalidSubscription) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "invalid_subscription")
            }
            Some(ErrorCode::ReminderTimeRequired) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "reminder_time_required")
            }
            Some(ErrorCode::RefreshInProgress) => (StatusCode::CONFLICT, "refresh_in_progress"),
            Some(ErrorCode::StaleRefresh) => (StatusCode::CONFLICT, "stale_refresh"),
            Some(ErrorCode::StaleDelivery) => (StatusCode::CONFLICT, "stale_delivery"),
            Some(ErrorCode::IntegrationUnconfigured) => {
                (StatusCode::SERVICE_UNAVAILABLE, "integration_unconfigured")
            }
            Some(ErrorCode::FetchFailed) => (StatusCode::BAD_GATEWAY, "fetch_failed"),
            Some(ErrorCode::MaterialisationRequired) => {
                (StatusCode::CONFLICT, "materialisation_required")
            }
            Some(ErrorCode::DefaultsChanged) => (StatusCode::CONFLICT, "defaults_changed"),
            Some(ErrorCode::LastManager) => (StatusCode::CONFLICT, "last_manager"),
            Some(ErrorCode::InvitationExpired) => (StatusCode::GONE, "invitation_expired"),
            Some(ErrorCode::InvalidValue)
            | Some(ErrorCode::IdentityGrantRequired)
            | Some(ErrorCode::BatchTooLarge) => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_value"),
            Some(ErrorCode::MalformedRequest) => (StatusCode::BAD_REQUEST, "malformed_request"),
            Some(ErrorCode::RateLimited) => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            Some(ErrorCode::DeviceCapacity) => (StatusCode::SERVICE_UNAVAILABLE, "device_capacity"),
            Some(ErrorCode::SliceCapacity) => (StatusCode::SERVICE_UNAVAILABLE, "slice_capacity"),
            Some(ErrorCode::NotReady) => (StatusCode::SERVICE_UNAVAILABLE, "not_ready"),
            _ if busy => (StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable"),
            _ if unique => (StatusCode::CONFLICT, "conflict"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        };
        let id = REQUEST_ID
            .try_with(Clone::clone)
            .unwrap_or_else(|_| Uuid::new_v4().to_string());
        let error_kind = if self.0.downcast_ref::<sqlx::Error>().is_some() {
            "database"
        } else {
            "application"
        };
        // Never log arbitrary error strings: database errors may contain user values.
        eprintln!(
            "{}",
            json!({"event":"request_error","request_id":id,"code":code,"error_kind":error_kind})
        );
        let message = match code {
            "unauthenticated" => "A valid session is required.",
            "forbidden" => "This action is not permitted.",
            "not_found" => "The resource is not available.",
            "rate_limited" => "Too many attempts. Please retry later.",
            "resync_required" => "Start a new synchronisation snapshot.",
            "conflict" | "operation_conflict" => "The request conflicts with existing state.",
            "invalid_value" | "malformed_request" => "Check the request fields and values.",
            _ => "The request could not be completed.",
        };
        let mut response = (
            status,
            Json(json!({"code":code,"message":message,"request_id":id,"details":[]})),
        )
            .into_response();
        if status == StatusCode::TOO_MANY_REQUESTS {
            response
                .headers_mut()
                .insert("retry-after", "60".parse().unwrap());
        }
        response
    }
}
