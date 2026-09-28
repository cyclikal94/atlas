use anyhow::Result;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::Value;
use tower::ServiceExt;
pub(crate) async fn session_request(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/api/experimental/v1/{path}"))
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        },
    )
}

pub(crate) async fn command_request(
    app: &Router,
    path: &str,
    token: Option<&str>,
    operation: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut r = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(format!("/api/experimental/v1/{path}"))
        .header("content-type", "application/json");
    if let Some(t) = token {
        r = r.header("authorization", format!("Bearer {t}"));
    }
    if let Some(op) = operation {
        r = r.header("idempotency-key", op);
    }
    let response = app
        .clone()
        .oneshot(
            r.body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// A request with arbitrary headers (repeated names allowed), returning the status, headers and
/// JSON body (`Null` when empty).
pub(crate) async fn request(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Value,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/api/experimental/v1/{path}"))
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        headers,
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        },
    )
}
pub(crate) const DELAYED_FEED_ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:trip\r\nDTSTART:20260910T090000Z\r\nSUMMARY:Trip\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
/// A loopback feed that answers every request immediately with whatever body the returned handle
/// currently holds — unlike `delayed_feed`, there is no mid-flight window; this is for cases
/// that need real fetches to a controlled response body that a test changes between attempts
/// (BE-B8 revision: a linked refresh that first succeeds, then meets content that itself fails,
/// then meets corrected content).
pub(crate) async fn switchable_feed(
    initial: String,
) -> Result<(
    String,
    std::sync::Arc<std::sync::Mutex<String>>,
    tokio::task::JoinHandle<std::io::Result<()>>,
)> {
    use axum::routing::get as feed_get;
    use std::sync::{Arc, Mutex};
    let body = Arc::new(Mutex::new(initial));
    let served = body.clone();
    let feed = Router::new().route(
        "/feed",
        feed_get(move || {
            let served = served.clone();
            async move { served.lock().unwrap().clone() }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, feed).await });
    Ok((origin, body, server))
}
/// A loopback feed that signals arrival on `arrived`, then blocks until released (the sender
/// this returns), before answering with a valid ICS body. Lets a test perform a mid-flight
/// action (a settings edit, a lease/archive change) between "the fetch has started" and "the
/// fetch succeeds", so `finish_calendar_refresh_receipt`'s guards see state that changed after
/// the attempt began — real HTTP, no test-only accessor into `worker.rs`.
pub(crate) async fn delayed_feed() -> Result<(
    String,
    tokio::sync::mpsc::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<std::io::Result<()>>,
)> {
    use axum::routing::get as feed_get;
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc, oneshot};
    let (arrived_tx, arrived_rx) = mpsc::channel::<()>(1);
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));
    let feed = Router::new().route(
        "/feed",
        feed_get(move || {
            let arrived_tx = arrived_tx.clone();
            let release_rx = release_rx.clone();
            async move {
                arrived_tx.send(()).await.ok();
                if let Some(receiver) = release_rx.lock().await.take() {
                    let _ = receiver.await;
                }
                DELAYED_FEED_ICS
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, feed).await });
    Ok((origin, arrived_rx, release_tx, server))
}
