//! Short-lived, single-use browser sign-in. The owner credential stays in the
//! companion; only an ephemeral capability is placed in a URL fragment.
use crate::state::AppState;
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct Handoffs(HashMap<String, Instant>);

fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn failure(status: StatusCode) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"error":"browser_sign_in_unavailable"})),
    )
        .into_response()
}

pub async fn issue(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // Sessions and family invitations cannot mint owner capabilities.
    if !state.auth.bearer_authorized(&headers) {
        return failure(StatusCode::UNAUTHORIZED);
    }
    let mut entries = state.browser_handoffs.lock().await;
    let now = Instant::now();
    entries.0.retain(|_, expires| *expires > now);
    if entries.0.len() >= 32 {
        return failure(StatusCode::TOO_MANY_REQUESTS);
    }
    let code = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    entries
        .0
        .insert(digest(&code), now + Duration::from_secs(60));
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"code":code,"expires_in":60})),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct Consume {
    code: String,
}

fn same_origin(headers: &HeaderMap) -> bool {
    if headers
        .get("sec-fetch-site")
        .is_some_and(|v| v != "same-origin" && v != "none")
    {
        return false;
    }
    // JSON + no CORS already prevents cross-origin browser POSTs. Check Origin
    // and Fetch Metadata as well, including same-site sibling hosts/ports.
    if let Some(origin) = headers.get(header::ORIGIN) {
        let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
            return false;
        };
        let scheme = if headers
            .get("x-forwarded-proto")
            .is_some_and(|v| v == "https")
        {
            "https"
        } else {
            "http"
        };
        let expected = url::Url::parse(&format!("{scheme}://{host}"));
        let presented = origin.to_str().ok().and_then(|v| url::Url::parse(v).ok());
        return match (expected, presented) {
            (Ok(expected), Some(presented)) => expected.origin() == presented.origin(),
            _ => false,
        };
    }
    true
}

pub async fn consume(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Consume>,
) -> Response {
    if !same_origin(&headers) {
        return failure(StatusCode::FORBIDDEN);
    }
    if input.code.len() != 64 || !input.code.bytes().all(|b| b.is_ascii_hexdigit()) {
        return failure(StatusCode::UNAUTHORIZED);
    }
    let mut entries = state.browser_handoffs.lock().await;
    let Some(expiry) = entries.0.remove(&digest(&input.code)) else {
        return failure(StatusCode::UNAUTHORIZED);
    };
    if expiry <= Instant::now() {
        return failure(StatusCode::UNAUTHORIZED);
    }
    // Consumption is atomic. A failed database write requires a fresh handoff;
    // a code can never be replayed after an ambiguous response.
    let db = state.db.lock().await;
    if crate::household::initialize(&db).is_err() {
        return failure(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let session = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    if db
        .conn()
        .execute(
            "INSERT INTO household_sessions(token_hash,member_id,expires) VALUES(?1,NULL,?2)",
            rusqlite::params![
                digest(&session),
                chrono::Utc::now().timestamp() + 86400 * 30
            ],
        )
        .is_err()
    {
        return failure(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let secure = if headers
        .get("x-forwarded-proto")
        .is_some_and(|v| v == "https")
    {
        "; Secure"
    } else {
        ""
    };
    ([(header::SET_COOKIE, format!("travel_session={session}; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000{secure}")), (header::CACHE_CONTROL, "no-store".into())], Json(json!({"destination":"/travel"}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_cross_origin_and_cross_site_posts() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "localhost:3150".parse().unwrap());
        headers.insert(header::ORIGIN, "http://localhost:3150".parse().unwrap());
        assert!(same_origin(&headers));
        headers.insert(header::ORIGIN, "http://localhost:4000".parse().unwrap());
        assert!(!same_origin(&headers));
        headers.insert(header::ORIGIN, "https://attacker.example".parse().unwrap());
        assert!(!same_origin(&headers));
        headers.remove(header::ORIGIN);
        headers.insert("sec-fetch-site", "cross-site".parse().unwrap());
        assert!(!same_origin(&headers));
    }

    #[tokio::test]
    async fn expired_handoff_is_rejected() {
        let state = AppState::new(
            envelope_email_store::Database::open_memory().unwrap(),
            envelope_email_store::CredentialBackend::File,
        );
        let code = "a".repeat(64);
        state
            .browser_handoffs
            .lock()
            .await
            .0
            .insert(digest(&code), Instant::now() - Duration::from_secs(1));
        assert_eq!(
            consume(State(state), HeaderMap::new(), Json(Consume { code }))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
}
