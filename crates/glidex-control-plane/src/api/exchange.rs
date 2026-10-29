//! `POST /auth/exchange` (spec/19-tenancy.md §9): RFC 8693's token exchange,
//! form or JSON, public. The work is `directory::exchange`.

use super::{AppState, RequestId};
use crate::directory::exchange::{self as x, Request};
use axum::{
    Extension,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use std::collections::HashMap;

/// `application/x-www-form-urlencoded` fields.
fn form(s: &str) -> HashMap<String, String> {
    let dec = |v: &str| {
        percent_encoding::percent_decode_str(&v.replace('+', " "))
            .decode_utf8_lossy()
            .into_owned()
    };
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (dec(k), dec(v))
        })
        .collect()
}

fn request_of(headers: &HeaderMap, body: &Bytes) -> Option<Request> {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.starts_with("application/json"));
    let fields: HashMap<String, String> = if json {
        let v: serde_json::Value = serde_json::from_slice(body).ok()?;
        v.as_object()?
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
            .collect()
    } else {
        form(std::str::from_utf8(body).ok()?)
    };
    let get = |k: &str| fields.get(k).cloned().unwrap_or_default();
    Some(Request {
        grant_type: get("grant_type"),
        subject_token: get("subject_token"),
        subject_token_type: get("subject_token_type"),
        federation: get("federation"),
    })
}

pub async fn exchange(
    State(app): State<AppState>,
    Extension(RequestId(rid)): Extension<RequestId>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let answer = match request_of(&headers, &body) {
        Some(req) => x::exchange(&app, &req, &rid).await,
        None => x::Answer::Malformed("a form or a JSON object".into()),
    };
    let mut r = (
        StatusCode::from_u16(answer.status()).unwrap_or(StatusCode::BAD_REQUEST),
        axum::Json(answer.body()),
    )
        .into_response();
    // RFC 6749 §5.1: tokens are never cached.
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    r
}
