//! Optional Web Push with per-device subscriptions and a persistent outbox.
use crate::{household::Member, state::AppState};
use anyhow::{Result, ensure};
use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use envelope_email_store::{Database, credential_store, crypto};
use rusqlite::OptionalExtension;
use serde_json::json;
use web_push::{ContentEncoding, SubscriptionInfo, VapidSignatureBuilder, WebPushMessageBuilder};

pub fn initialize(db: &Database) -> Result<()> {
    db.conn().execute_batch("CREATE TABLE IF NOT EXISTS push_configuration(id INTEGER PRIMARY KEY CHECK(id=1),private_key TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS push_devices(id TEXT PRIMARY KEY,member_id TEXT,subscription TEXT NOT NULL,enabled INTEGER NOT NULL DEFAULT 1);
    CREATE TABLE IF NOT EXISTS push_outbox(id TEXT PRIMARY KEY,device_id TEXT NOT NULL,alert_id TEXT NOT NULL,payload TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'queued',attempts INTEGER NOT NULL DEFAULT 0,next_attempt INTEGER NOT NULL,expires INTEGER NOT NULL,UNIQUE(device_id,alert_id));")?;
    Ok(())
}
fn valid_endpoint(endpoint: &str) -> bool {
    let Ok(url) = url::Url::parse(endpoint) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && (host == "fcm.googleapis.com"
            || host == "updates.push.services.mozilla.com"
            || host == "web.push.apple.com"
            || host.ends_with(".push.apple.com"))
}
async fn key(state: &AppState, create: bool) -> Result<String> {
    let pass = credential_store::get_or_create_passphrase(state.backend)?;
    let db = state.db.lock().await;
    initialize(&db)?;
    if let Some(encrypted) = db
        .conn()
        .query_row(
            "SELECT private_key FROM push_configuration WHERE id=1",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(crypto::decrypt(&encrypted, &pass)?);
    }
    ensure!(create, "Push is disabled");
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let secret = URL_SAFE_NO_PAD.encode(bytes);
    VapidSignatureBuilder::from_base64_no_sub(&secret)?;
    db.conn().execute(
        "INSERT INTO push_configuration VALUES(1,?1)",
        [crypto::encrypt(&secret, &pass)?],
    )?;
    Ok(secret)
}
pub async fn enable(State(state): State<AppState>) -> Response {
    match key(&state, true).await.and_then(|key| {
        Ok(URL_SAFE_NO_PAD
            .encode(VapidSignatureBuilder::from_base64_no_sub(&key)?.get_public_key()))
    }) {
        Ok(public) => Json(json!({"public_key":public})).into_response(),
        Err(_) => StatusCode::BAD_REQUEST.into_response(),
    }
}
pub async fn subscribe(
    State(state): State<AppState>,
    member: Option<Extension<Member>>,
    Json(subscription): Json<SubscriptionInfo>,
) -> Response {
    if !valid_endpoint(&subscription.endpoint) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"unsupported_push_service"})),
        )
            .into_response();
    }
    if key(&state, false).await.is_err() {
        return StatusCode::CONFLICT.into_response();
    }
    let id = uuid::Uuid::new_v4().to_string();
    let db = state.db.lock().await;
    // Re-enabling a browser replaces its own subscription, not another member's.
    let owner = member.as_ref().map(|m| m.0.id.as_str());
    let encoded = serde_json::to_string(&subscription).unwrap();
    let existing = db
        .conn()
        .query_row(
            "SELECT id FROM push_devices WHERE member_id IS ?1 AND subscription=?2",
            rusqlite::params![owner, encoded],
            |r| r.get::<_, String>(0),
        )
        .optional();
    if let Ok(Some(existing)) = existing {
        if db
            .conn()
            .execute("UPDATE push_devices SET enabled=1 WHERE id=?1", [&existing])
            .is_err()
        {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        return Json(json!({"id":existing})).into_response();
    }
    match db.conn().execute(
        "INSERT INTO push_devices(id,member_id,subscription) VALUES(?1,?2,?3)",
        rusqlite::params![
            id,
            member.map(|m| m.0.id),
            serde_json::to_string(&subscription).unwrap()
        ],
    ) {
        Ok(_) => Json(json!({"id":id})).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
pub async fn remove(
    State(state): State<AppState>,
    member: Option<Extension<Member>>,
    Path(id): Path<String>,
) -> Response {
    let db = state.db.lock().await;
    let member = member.map(|m| m.0.id);
    match db.conn().execute(
        "UPDATE push_devices SET enabled=0 WHERE id=?1 AND member_id IS ?2",
        rusqlite::params![id, member],
    ) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => StatusCode::BAD_REQUEST.into_response(),
    }
}
pub async fn test(
    State(state): State<AppState>,
    member: Option<Extension<Member>>,
    Path(id): Path<String>,
) -> Response {
    let db = state.db.lock().await;
    let member = member.map(|m| m.0.id);
    let exists=db.conn().query_row("SELECT EXISTS(SELECT 1 FROM push_devices WHERE id=?1 AND member_id IS ?2 AND enabled=1)",rusqlite::params![id,member],|r|r.get::<_,bool>(0)).unwrap_or(false);
    if !exists {
        return StatusCode::NOT_FOUND.into_response();
    }
    let now = chrono::Utc::now().timestamp();
    let job = uuid::Uuid::new_v4().to_string();
    match db.conn().execute("INSERT INTO push_outbox(id,device_id,alert_id,payload,next_attempt,expires) VALUES(?1,?2,?1,?3,?4,?5)",rusqlite::params![job,id,json!({"title":"Travel notifications enabled","body":"This is your test notification.","url":if member.is_some(){"/household"}else{"/travel"}}).to_string(),now,now+300]){Ok(_)=>Json(json!({"status":"queued"})).into_response(),Err(_)=>StatusCode::INTERNAL_SERVER_ERROR.into_response()}
}
pub async fn deliver(state: &AppState) -> Result<()> {
    let enabled = {
        let db = state.db.lock().await;
        initialize(&db)?;
        db.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM push_devices WHERE enabled=1)",
            [],
            |r| r.get::<_, bool>(0),
        )?
    };
    if !enabled {
        return Ok(());
    }
    let _lock = state
        .lock_travel_receipt_operation("push-delivery-global")
        .await;
    let secret = key(state, false).await?;
    let now = chrono::Utc::now().timestamp();
    let devices = {
        let db = state.db.lock().await;
        let mut stmt = db
            .conn()
            .prepare("SELECT id,member_id,subscription FROM push_devices WHERE enabled=1")?;
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for (device, member, encoded) in devices {
        let mut allowed: Option<Vec<String>> = None;
        if let Some(member) = &member {
            let db = state.db.lock().await;
            let trips = db
                .conn()
                .query_row(
                    "SELECT trips FROM household_members WHERE id=?1 AND revoked=0",
                    [member],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            let Some(trips) = trips else { continue };
            allowed = Some(serde_json::from_str(&trips)?);
        }
        let jobs = {
            let db = state.db.lock().await;
            for alert in db.list_travel_alerts(None)? {
                if alert.acknowledged_at.is_some() || alert.triggered_at.is_none() {
                    continue;
                }
                if allowed.as_ref().is_some_and(|trips| {
                    !alert.trip_id.as_ref().is_some_and(|id| trips.contains(id))
                }) {
                    continue;
                }
                let triggered =
                    chrono::DateTime::parse_from_rfc3339(alert.triggered_at.as_ref().unwrap())?
                        .timestamp();
                if triggered + 3600 < now {
                    continue;
                }
                db.conn().execute("INSERT OR IGNORE INTO push_outbox(id,device_id,alert_id,payload,next_attempt,expires) VALUES(?1,?2,?3,?4,?5,?6)",rusqlite::params![uuid::Uuid::new_v4().to_string(),device,alert.id,json!({"title":"Travel update","body":"Open your trip to review a new alert.","url":if member.is_some(){"/household"}else{"/travel"}}).to_string(),now,triggered+3600])?;
            }
            db.conn().execute(
                "UPDATE push_outbox SET state='expired' WHERE state='queued' AND expires<=?1",
                [now],
            )?;
            db.conn().execute(
                "UPDATE push_outbox SET state='queued' WHERE state='sending' AND next_attempt<=?1",
                [now],
            )?;
            let mut stmt=db.conn().prepare("SELECT id,payload,attempts,alert_id FROM push_outbox WHERE device_id=?1 AND state='queued' AND next_attempt<=?2 AND expires>?2 LIMIT 20")?;
            stmt.query_map(rusqlite::params![device, now], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let subscription: SubscriptionInfo = serde_json::from_str(&encoded)?;
        ensure!(
            valid_endpoint(&subscription.endpoint),
            "Unsupported push service"
        );
        for (id, payload, attempts, alert_id) in jobs {
            {
                let db = state.db.lock().await;
                // Check current scope and acknowledgment immediately before delivery.
                // Test notifications use their own outbox ID, not a reservation alert.
                let active=db.conn().query_row("SELECT EXISTS(SELECT 1 FROM push_devices d WHERE d.id=?1 AND d.enabled=1 AND (d.member_id IS NULL OR EXISTS(SELECT 1 FROM household_members m WHERE m.id=d.member_id AND m.revoked=0)))",[&device],|r|r.get::<_,bool>(0))?;
                let permitted = if id == alert_id {
                    active
                } else {
                    active && db.conn().query_row("SELECT EXISTS(SELECT 1 FROM travel_alerts a JOIN push_devices d ON d.id=?1 WHERE a.id=?2 AND a.acknowledged_at IS NULL AND a.triggered_at IS NOT NULL AND (d.member_id IS NULL OR EXISTS(SELECT 1 FROM household_members m,json_each(m.trips) t WHERE m.id=d.member_id AND m.revoked=0 AND t.value=a.trip_id)))",rusqlite::params![device,alert_id],|r|r.get::<_,bool>(0))?
                };
                if !permitted {
                    db.conn().execute(
                        "UPDATE push_outbox SET state='superseded' WHERE id=?1 AND state='queued'",
                        [&id],
                    )?;
                    continue;
                }
                // A lease prevents two service processes from sending the same queued job.
                if db.conn().execute("UPDATE push_outbox SET state='sending',next_attempt=?2 WHERE id=?1 AND state='queued'",rusqlite::params![id,now+120])?!=1{continue}
            }
            let result = send(&secret, &subscription, payload.as_bytes()).await;
            let db = state.db.lock().await;
            if matches!(result, Ok(404 | 410)) {
                db.conn()
                    .execute("UPDATE push_devices SET enabled=0 WHERE id=?1", [&device])?;
            }
            let status = if result
                .as_ref()
                .is_ok_and(|status| (200..300).contains(status))
            {
                "accepted"
            } else if attempts >= 4 || matches!(result, Ok(404 | 410)) {
                "failed"
            } else {
                "queued"
            };
            db.conn().execute(
                "UPDATE push_outbox SET state=?2,attempts=attempts+1,next_attempt=?3 WHERE id=?1",
                rusqlite::params![id, status, now + 60 * (1 << attempts.min(5))],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn push_endpoints_cannot_target_arbitrary_servers() {
        assert!(valid_endpoint("https://fcm.googleapis.com/fcm/send/test"));
        assert!(valid_endpoint("https://web.push.apple.com/test"));
        for endpoint in [
            "http://fcm.googleapis.com/test",
            "https://fcm.googleapis.com.evil.example/test",
            "https://localhost/test",
            "https://127.0.0.1/test",
            "https://user@web.push.apple.com/test",
            "https://web.push.apple.com:444/test",
        ] {
            assert!(!valid_endpoint(endpoint), "{endpoint}");
        }
    }
}
async fn send(secret: &str, subscription: &SubscriptionInfo, content: &[u8]) -> Result<u16> {
    let signature = VapidSignatureBuilder::from_base64(secret, subscription)?.build()?;
    let mut builder = WebPushMessageBuilder::new(subscription);
    builder.set_ttl(300);
    builder.set_payload(ContentEncoding::Aes128Gcm, content);
    builder.set_vapid_signature(signature);
    let message = builder.build()?;
    let payload = message.payload.unwrap();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let mut request = client
        .post(subscription.endpoint.clone())
        .header("TTL", "300")
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream");
    for (name, value) in payload.crypto_headers {
        request = request.header(name, value);
    }
    Ok(request
        .body(payload.content)
        .send()
        .await?
        .status()
        .as_u16())
}
