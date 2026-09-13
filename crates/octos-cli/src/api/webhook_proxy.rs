//! Reverse-proxy helpers for delegating API calls to a gateway process.
//!
//! The channel webhook surface (`/webhook/{feishu,line,dingtalk,twilio}/…`)
//! was removed with the multi-channel gateway; what remains are the generic
//! `api_*_proxy` helpers the WS transport and REST handlers use when the
//! API server defers task ownership to a separate gateway process.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use super::AppState;

/// Proxy a GET request to the gateway's API channel.
pub async fn api_get_proxy(state: &AppState, port: u16, path: &str) -> Response {
    let url = format!("http://127.0.0.1:{port}{path}");
    let resp = match state.http_client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(port, error = %e, "API GET proxy failed");
            return json_error(
                StatusCode::BAD_GATEWAY,
                &format!("gateway proxy failed: {e}"),
            );
        }
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = resp.bytes().await.unwrap_or_default();
    let mut response = (status, body.to_vec()).into_response();
    response
        .headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    response
}

/// Proxy a POST request (with optional JSON body) to the gateway's API
/// channel. The response status and body are forwarded verbatim — used
/// by the M7.9 cancel / restart-from-node endpoints so the API server
/// can hand control back to the gateway process that owns the supervisor.
pub async fn api_post_proxy_json(
    state: &AppState,
    port: u16,
    path: &str,
    body: serde_json::Value,
) -> Response {
    let url = format!("http://127.0.0.1:{port}{path}");
    let resp = match state.http_client.post(&url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(port, error = %e, "API POST proxy failed");
            return json_error(
                StatusCode::BAD_GATEWAY,
                &format!("gateway proxy failed: {e}"),
            );
        }
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = resp.bytes().await.unwrap_or_default();
    let mut response = (status, body.to_vec()).into_response();
    response
        .headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    response
}

/// Proxy a PATCH request with a JSON body to the gateway's API channel.
pub async fn api_patch_proxy(state: &AppState, port: u16, path: &str, body: String) -> Response {
    let url = format!("http://127.0.0.1:{port}{path}");
    let resp = match state
        .http_client
        .patch(&url)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(port, error = %e, "API PATCH proxy failed");
            return json_error(
                StatusCode::BAD_GATEWAY,
                &format!("gateway proxy failed: {e}"),
            );
        }
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    status.into_response()
}

/// Proxy a DELETE request to the gateway's API channel.
pub async fn api_delete_proxy(state: &AppState, port: u16, path: &str) -> Response {
    let url = format!("http://127.0.0.1:{port}{path}");
    let resp = match state.http_client.delete(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(port, error = %e, "API DELETE proxy failed");
            return json_error(
                StatusCode::BAD_GATEWAY,
                &format!("gateway proxy failed: {e}"),
            );
        }
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    status.into_response()
}

/// Return a JSON error response.
fn json_error(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({"error": message});
    (status, axum::Json(body)).into_response()
}
