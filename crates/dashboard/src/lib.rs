//! Standalone Travel HTTP service. No mail send, move, or delete routes.
pub mod assets;
pub mod auth;
pub mod backup;
pub mod csrf;
pub mod documents;
pub mod events;
pub mod handlers;
pub mod household;
pub mod intelligence;
pub mod learning;
pub mod migration;
pub mod oauth;
pub mod push;
pub mod state;
pub mod timefmt;
pub mod travel_ics;
pub mod travel_parser;
pub mod workspace;

use axum::{
    Router,
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use handlers::travel as t;
use state::AppState;

pub fn dashboard_router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/v1/parsers", get(learning::versions))
        .route("/v1/parsers/{id}/restore", post(learning::restore_version))
        .route("/v1/household/push/enable", post(push::enable))
        .route("/v1/household/push/devices", post(push::subscribe))
        .route("/v1/household/push/devices/{id}", delete(push::remove))
        .route("/v1/household/push/devices/{id}/test", post(push::test))
        .route(
            "/v1/intelligence",
            get(intelligence::get_configuration).put(intelligence::put_configuration),
        )
        .route("/v1/members", post(household::invite))
        .route("/v1/members/{id}", delete(household::revoke))
        .route("/v1/household/overview", get(household::overview))
        .route("/v1/household/trips/{id}", put(household::edit))
        .route("/v1/household/tasks/{id}/toggle", post(household::toggle))
        .route("/v1/sources/gmail/authorize", post(oauth::start))
        .route("/v1/native/projection", get(workspace::native))
        .route("/v1/calendar.ics", get(workspace::calendar))
        .route("/v1/tasks/{id}/completion", put(workspace::complete))
        .route("/v1/search", get(workspace::search))
        .route("/v1/bookings/{id}/links", post(workspace::link))
        .route("/v1/bookings/{id}", put(workspace::edit_booking))
        .route("/v1/trips/{id}", put(workspace::edit_trip))
        .route(
            "/v1/trips/{id}/move-bookings",
            post(workspace::move_bookings),
        )
        .route("/v1/documents/import/{filename}", post(documents::import))
        .route("/v1/documents/{hash}", get(documents::download))
        .route("/travel/overview", get(t::overview))
        .route("/travel/connect-gmail", post(t::connect_gmail))
        .route("/travel/settings", put(t::put_settings))
        .route("/travel/sync", post(t::sync))
        .route("/travel/import", post(t::import_receipt))
        .route(
            "/travel/receipts/{receipt_id}/approve",
            post(t::approve_receipt),
        )
        .route(
            "/travel/receipts/{receipt_id}/dismiss",
            post(t::dismiss_receipt),
        )
        .route("/travel/trips", post(t::create_trip))
        .route("/travel/bookings", post(t::create_booking))
        .route("/travel/trips/{trip_id}/tasks", post(t::create_task))
        .route("/travel/tasks/{task_id}/toggle", post(t::toggle_task))
        .route(
            "/travel/alerts/{alert_id}/acknowledge",
            post(t::acknowledge_alert),
        )
        .route("/travel/shares", post(t::create_share))
        .route("/travel/shares/{share_id}", delete(t::revoke_share))
        .route("/csrf", get(csrf::issue))
        .route_layer(axum::middleware::from_fn(csrf::require_csrf))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            household::authorize,
        ));
    let api = Router::new()
        .route("/v1/session", post(household::session))
        .route("/v1/oauth/gmail/callback", get(oauth::callback))
        .route("/health", get(|| async { axum::Json(serde_json::json!({"product":"Travel","status":"ok","version":env!("CARGO_PKG_VERSION")})) }))
        .route("/public/travel/{token}", get(t::public_trip))
        .route("/public/travel/{token}/calendar.ics", get(t::public_calendar))
        .route("/public/travel/{token}/tasks/{task_id}/toggle", post(t::public_toggle_task))
        .merge(protected)
        .fallback(|| async {(StatusCode::NOT_FOUND, axum::Json(serde_json::json!({"error":"not_found"})))});
    Router::new()
        .nest("/api", api)
        .fallback(assets)
        .layer(axum::extract::DefaultBodyLimit::max(20 * 1024 * 1024))
        .layer(axum::middleware::from_fn_with_state(state.clone(), local_host_boundary))
        .with_state(state)
}

async fn local_host_boundary(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // Same-origin CSRF alone cannot prevent DNS rebinding into an open local
    // instance. Non-local hostnames require an explicitly configured token.
    if !state.auth.is_enforced() {
        if let Some(host) = request.headers().get(header::HOST) {
            let valid = host.to_str().ok().and_then(|host|format!("http://{host}").parse::<url::Url>().ok())
                .is_some_and(|url| matches!(url.host_str(),Some("localhost"|"127.0.0.1"|"[::1]"|"::1")) && url.username().is_empty() && url.password().is_none());
            if !valid { return (StatusCode::FORBIDDEN, "Use localhost, or configure TRAVEL_TOKEN for remote access.").into_response(); }
        }
    }
    next.run(request).await
}

async fn assets(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let shell = path.is_empty()
        || matches!(path, "travel" | "household" | "settings")
        || path.starts_with("share/");
    let asset = assets::WebAssets::get(path).or_else(|| {
        if shell {
            assets::WebAssets::get("index.html")
        } else {
            None
        }
    });
    match asset {
        Some(file) => {
            let mime = if shell {
                "text/html".to_string()
            } else {
                mime_guess::from_path(path)
                    .first_or_octet_stream()
                    .to_string()
            };
            (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, "no-cache".to_string()),
                ],
                file.data.into_owned(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn run_listener(
    listener: tokio::net::TcpListener,
    state: AppState,
) -> anyhow::Result<()> {
    axum::serve(listener, dashboard_router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
