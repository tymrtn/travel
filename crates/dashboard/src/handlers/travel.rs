// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Envelope Travel dashboard API and read-only mailbox ingestion.

use std::collections::HashSet;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode, header};
use axum::response::IntoResponse;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use envelope_email_store::{
    Account, AccountWithCredentials, MessageSummary, NewTravelAlert, NewTravelBooking,
    NewTravelIngestFailure, NewTravelReceipt, NewTravelSegment, NewTravelShareLink, NewTravelTask,
    NewTrip, TravelBooking, TravelBookingUpdate, TravelIngestCursor, TravelMessageSourceKey,
    TravelReceipt, TravelReceiptAtomicTransition, TravelReceiptIngestResult,
    TravelReceiptReviewTransition, TravelReceiptSourceOrder, TravelReceiptTerminalTransition,
    TravelSegmentUpdate, TravelSettings, TripUpdate, compare_travel_receipt_sources,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::state::AppState;
use crate::travel_ics::{TravelCalendarEvent, render_travel_calendar};
use crate::travel_parser::{
    ParsedTravelDocument, classify_candidate, parse_raw_travel_email, parse_travel_document,
};

const DEFAULT_SYNC_LIMIT: u32 = 250;
const MAX_SYNC_LIMIT: u32 = 1000;
const RECEIPT_LIST_LIMIT: usize = 500;
const MAX_MESSAGE_INGEST_ATTEMPTS: i64 = 3;
const MAX_INGEST_ERROR_CHARS: usize = 500;

#[derive(Debug)]
enum PersistTravelError {
    Message(String),
    ReceiptConflict(TravelReceipt),
}

impl std::fmt::Display for PersistTravelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(message) => formatter.write_str(message),
            Self::ReceiptConflict(receipt) => write!(
                formatter,
                "receipt {} is already {}",
                receipt.id, receipt.status
            ),
        }
    }
}

impl From<String> for PersistTravelError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

fn api_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response<Body> {
    (
        status,
        Json(json!({ "code": code, "message": message.into() })),
    )
        .into_response()
}

fn db_error(error: impl std::fmt::Display) -> Response<Body> {
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "travel_store_error",
        format!("travel data error: {error}"),
    )
}

fn receipt_state_conflict(receipt: TravelReceipt) -> Response<Body> {
    let status = receipt.status.clone();
    (
        StatusCode::CONFLICT,
        Json(json!({
            "code": "travel_receipt_state_conflict",
            "message": format!("This receipt is already {status}. Refresh Travel before trying again."),
            "current_status": status,
            "receipt": receipt_view(receipt),
        })),
    )
        .into_response()
}

#[derive(Debug, Serialize)]
struct OverviewResponse {
    gmail_onboarding_secure: bool,
    settings: Option<TravelSettings>,
    sync: Option<OverviewSync>,
    accounts: Vec<envelope_email_store::Account>,
    trips: Vec<envelope_email_store::Trip>,
    receipts: Vec<Value>,
    bookings: Vec<Value>,
    segments: Vec<envelope_email_store::TravelSegment>,
    tasks: Vec<envelope_email_store::TravelTask>,
    alerts: Vec<envelope_email_store::TravelAlert>,
    shares: Vec<envelope_email_store::TravelShareLink>,
    generated_at: String,
}

#[derive(Debug, Serialize)]
struct OverviewSync {
    last_sync_at: String,
    last_message_at: Option<String>,
}

pub async fn overview(State(state): State<AppState>, headers: HeaderMap) -> Response<Body> {
    let db = state.db.lock().await;
    let result = (|| -> envelope_email_store::errors::Result<OverviewResponse> {
        let settings = db.get_travel_settings()?;
        let sync = settings
            .as_ref()
            .and_then(|value| {
                value
                    .account_id
                    .as_deref()
                    .map(|account_id| (account_id, value.scan_folder.as_str()))
            })
            .map(|(account_id, folder)| db.get_travel_cursor(account_id, folder))
            .transpose()?
            .flatten()
            .map(|cursor| OverviewSync {
                last_sync_at: cursor.updated_at,
                last_message_at: cursor.last_message_at,
            });
        let accounts = db.list_accounts()?;
        let trips = db.list_trips()?;
        let receipt_rows = db.list_travel_receipts(None, true, RECEIPT_LIST_LIMIT)?;
        let mut bookings = Vec::new();
        let mut segments = Vec::new();
        let mut shares = Vec::new();
        for trip in &trips {
            bookings.extend(
                db.list_travel_bookings(&trip.id)?
                    .into_iter()
                    .map(booking_view),
            );
            segments.extend(db.list_travel_segments(&trip.id)?);
            shares.extend(db.list_travel_share_links(&trip.id)?);
        }
        let receipts = receipt_rows.into_iter().map(receipt_view).collect();
        Ok(OverviewResponse {
            gmail_onboarding_secure: gmail_onboarding_transport_is_secure(&state, &headers),
            settings,
            sync,
            accounts,
            trips,
            receipts,
            bookings,
            segments,
            tasks: db.list_travel_tasks(None)?,
            alerts: db.list_travel_alerts(None)?,
            shares,
            generated_at: now(),
        })
    })();
    match result {
        Ok(data) => Json(data).into_response(),
        Err(error) => db_error(error),
    }
}

fn mask_confirmation(value: Option<&str>) -> Option<String> {
    value.map(|code| {
        let chars: Vec<char> = code.chars().collect();
        if chars.len() <= 4 {
            return "••••".to_string();
        }
        let visible = (chars.len() - 1).min(4);
        let suffix: String = chars[chars.len() - visible..].iter().collect();
        format!("•••• {suffix}")
    })
}

fn booking_view(booking: TravelBooking) -> Value {
    json!({
        "id": booking.id,
        "trip_id": booking.trip_id,
        "receipt_id": booking.receipt_id,
        "kind": booking.kind,
        "provider": booking.provider,
        "title": booking.title,
        "confirmation_masked": mask_confirmation(booking.confirmation_code.as_deref()),
        "status": booking.status,
        "starts_at": booking.starts_at,
        "ends_at": booking.ends_at,
        "location": booking.location,
        "created_at": booking.created_at,
        "updated_at": booking.updated_at,
    })
}

fn receipt_view(receipt: TravelReceipt) -> Value {
    let parsed = receipt
        .extracted
        .as_ref()
        .and_then(|value| serde_json::from_value::<ParsedTravelDocument>(value.clone()).ok());
    let excerpt = receipt
        .body_text
        .as_deref()
        .map(|body| body.chars().take(280).collect::<String>())
        .map(|excerpt| {
            let excerpt = redact_confirmation_labels(&excerpt);
            parsed
                .as_ref()
                .and_then(|value| value.confirmation_code.as_deref())
                .map(|code| redact_literal(&excerpt, code))
                .unwrap_or(excerpt)
        });
    json!({
        "id": receipt.id,
        "account_id": receipt.account_id,
        "folder": receipt.folder,
        "uidvalidity": receipt.uidvalidity,
        "uid": receipt.uid,
        "message_id": receipt.message_id,
        "sender": receipt.from_addr,
        "subject": receipt.subject,
        "received_at": receipt.received_at,
        "trip_id": receipt.trip_id,
        "status": receipt.status,
        "decision": parsed.as_ref().map(|p| p.decision.as_str()),
        "kind": parsed.as_ref().map(|p| p.kind.as_str()),
        "title": parsed.as_ref().map(|p| p.title.as_str()),
        "provider": parsed.as_ref().and_then(|p| p.provider.as_deref()),
        "parsed_status": parsed.as_ref().map(|p| p.status.as_str()),
        "start_at": parsed.as_ref().and_then(|p| p.start_at.as_deref()),
        "end_at": parsed.as_ref().and_then(|p| p.end_at.as_deref()),
        "timezone": parsed.as_ref().and_then(|p| p.timezone.as_deref()),
        "origin": parsed.as_ref().and_then(|p| p.origin.as_deref()),
        "destination": parsed.as_ref().and_then(|p| p.destination.as_deref()),
        "confidence": parsed.as_ref().map(|p| p.confidence),
        "confirmation_masked": parsed.as_ref().and_then(|p| mask_confirmation(p.confirmation_code.as_deref())),
        "quarantine_reason": receipt.quarantine_reason,
        "excerpt": excerpt,
        "created_at": receipt.created_at,
        "updated_at": receipt.updated_at,
    })
}

fn redact_literal(text: &str, secret: &str) -> String {
    if secret.trim().is_empty() {
        return text.to_string();
    }
    regex::RegexBuilder::new(&regex::escape(secret))
        .case_insensitive(true)
        .build()
        .map(|pattern| pattern.replace_all(text, "••••").into_owned())
        .unwrap_or_else(|_| text.to_string())
}

fn redact_confirmation_labels(text: &str) -> String {
    regex::RegexBuilder::new(
        r"(?P<label>\b(?:confirmation(?:\s+(?:code|number))?|record\s+locator|booking\s+(?:reference|code|number)|pnr)\b(?:\s*(?::|#)\s*|\s+is\s+|\s+))(?P<code>[[:alnum:]][[:alnum:]-]{2,})",
    )
    .case_insensitive(true)
    .build()
    .map(|pattern| pattern.replace_all(text, "${label}••••").into_owned())
    .unwrap_or_else(|_| text.to_string())
}

#[derive(Debug, Deserialize)]
pub struct SettingsRequest {
    pub account_id: Option<String>,
    pub scan_folder: Option<String>,
    pub home_timezone: Option<String>,
    pub calendar_name: Option<String>,
    pub auto_ingest: Option<bool>,
    pub default_alert_minutes: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ConnectGmailRequest {
    pub email: String,
    pub password: String,
    pub display_name: Option<String>,
    pub settings: Option<SettingsRequest>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
}

/// Verify a Gmail app password before committing either the credential or the
/// Travel selection. A rejected password never creates a dead account row, and
/// a valid retry safely replaces credentials for the same address.
pub async fn connect_gmail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ConnectGmailRequest>,
) -> Response<Body> {
    if !gmail_onboarding_transport_is_secure(&state, &headers) {
        return api_error(
            StatusCode::FORBIDDEN,
            "travel_gmail_secure_transport_required",
            "Gmail setup is disabled on a direct network listener. Run Envelope on loopback behind HTTPS or Tailscale Serve, then connect the mailbox there.",
        );
    }
    let email = request.email.trim().to_ascii_lowercase();
    let valid_address = email.split_once('@').is_some_and(|(local, domain)| {
        !local.is_empty() && !domain.is_empty() && !domain.contains('@')
    });
    if email.len() > 254
        || email
            .chars()
            .any(|character| matches!(character, '\r' | '\n' | '\0'))
        || !valid_address
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "travel_gmail_email_invalid",
            "Enter a valid Gmail or Google Workspace address.",
        );
    }
    let custom_host = request.imap_host.clone();
    let imap_host = custom_host.as_deref().unwrap_or("imap.gmail.com");
    if imap_host.trim().is_empty() || imap_host.chars().any(char::is_control) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_imap_host",
            "Enter an IMAP hostname",
        );
    }
    let imap_port = request.imap_port.unwrap_or(993);
    let password = if custom_host.is_some() {
        request.password.clone()
    } else {
        request
            .password
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
    };
    if password.is_empty() || password.len() > 1024 || (custom_host.is_none() && password.len() < 8)
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "travel_gmail_app_password_invalid",
            "Enter the app-specific password generated by Google.",
        );
    }

    let timestamp = now();
    let name = trim_option(request.display_name).unwrap_or_else(|| email.clone());
    let probe_credentials = AccountWithCredentials {
        account: Account {
            id: "travel-gmail-probe".into(),
            name: name.clone(),
            username: email.clone(),
            domain: email
                .split_once('@')
                .map(|(_, domain)| domain.to_string())
                .unwrap_or_default(),
            smtp_host: "smtp.gmail.com".into(),
            smtp_port: 587,
            imap_host: imap_host.into(),
            imap_port,
            smtp_username: None,
            imap_username: None,
            display_name: None,
            signature_text: None,
            signature_html: None,
            created_at: timestamp.clone(),
        },
        password: password.clone(),
        smtp_password: None,
        imap_password: None,
    };
    let mut probe = match envelope_email_transport::imap::connect(&probe_credentials).await {
        Ok(client) => client,
        Err(_) => {
            return api_error(
                StatusCode::UNAUTHORIZED,
                "travel_gmail_auth_failed",
                "Google rejected that app password. Create a fresh app password and try again.",
            );
        }
    };
    if envelope_email_transport::imap::examine_folder_info(&mut probe, "INBOX")
        .await
        .is_err()
    {
        return api_error(
            StatusCode::BAD_GATEWAY,
            "travel_gmail_verify_failed",
            "Gmail connected, but Envelope could not open the inbox read-only. Try again shortly.",
        );
    }
    drop(probe);

    let passphrase =
        match envelope_email_store::credential_store::get_or_create_passphrase(state.backend) {
            Ok(value) => value,
            Err(error) => {
                return api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "travel_credential_store_error",
                    format!("Credential store error: {error}"),
                );
            }
        };
    let preferences = request.settings.unwrap_or(SettingsRequest {
        account_id: None,
        scan_folder: None,
        home_timezone: None,
        calendar_name: None,
        auto_ingest: None,
        default_alert_minutes: None,
    });
    let (account, settings) = {
        let db = state.db.lock().await;
        let existing = match db.get_travel_settings() {
            Ok(value) => value,
            Err(error) => return db_error(error),
        };
        let scan_folder = preferences
            .scan_folder
            .as_deref()
            .or_else(|| existing.as_ref().map(|value| value.scan_folder.as_str()))
            .unwrap_or("INBOX");
        let scan_folder = match normalize_scan_folder(scan_folder) {
            Ok(value) => value,
            Err(message) => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "travel_scan_folder_invalid",
                    message,
                );
            }
        };
        let settings = TravelSettings {
            account_id: None,
            scan_folder,
            home_timezone: preferences
                .home_timezone
                .or_else(|| existing.as_ref().map(|value| value.home_timezone.clone()))
                .unwrap_or_else(|| "UTC".into()),
            calendar_name: preferences
                .calendar_name
                .or_else(|| existing.as_ref().map(|value| value.calendar_name.clone()))
                .unwrap_or_else(|| "Family travel".into()),
            auto_ingest: preferences
                .auto_ingest
                .or_else(|| existing.as_ref().map(|value| value.auto_ingest))
                .unwrap_or(true),
            default_alert_minutes: preferences
                .default_alert_minutes
                .or_else(|| existing.as_ref().map(|value| value.default_alert_minutes))
                .unwrap_or(120)
                .clamp(0, 60 * 24 * 30),
            updated_at: timestamp,
        };
        match db.upsert_imap_account_and_travel_settings(
            &name,
            &email,
            &password,
            &passphrase,
            &settings,
            imap_host,
            imap_port,
        ) {
            Ok(value) => value,
            Err(error) => return db_error(error),
        }
    };
    state.evict_imap(&account.id).await;
    Json(json!({
        "account": account,
        "verification": { "ok": true, "imap": true, "smtp": false, "error": null },
        "settings": settings,
    }))
    .into_response()
}

/// Gmail app passwords are reusable secrets. Production only enables this
/// endpoint on a loopback listener, and each request must additionally be
/// either addressed to a loopback host or arrive through a proxy that reports
/// HTTPS. This catches accidental plaintext reverse-proxy exposure while still
/// supporting Tailscale Serve and a conventional local HTTPS proxy.
fn gmail_onboarding_transport_is_secure(state: &AppState, headers: &HeaderMap) -> bool {
    if !state.travel_secret_onboarding_allowed {
        return false;
    }

    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    if host_is_loopback(host) {
        return true;
    }

    let forwarded_https = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("https"));
    if !forwarded_https {
        return false;
    }

    // Browsers expose their effective scheme through Origin. When present it
    // must agree with the proxy's HTTPS claim; non-browser clients may omit it.
    headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|origin| origin.trim().to_ascii_lowercase().starts_with("https://"))
}

fn host_is_loopback(host: &str) -> bool {
    let host = host.trim().trim_end_matches('.');
    if host.eq_ignore_ascii_case("localhost") || host.starts_with("localhost:") {
        return true;
    }
    if let Some(ipv6) = host.strip_prefix('[') {
        return ipv6.split_once(']').is_some_and(|(address, suffix)| {
            address == "::1" && (suffix.is_empty() || suffix.starts_with(':'))
        });
    }
    let address = host.split(':').next().unwrap_or(host);
    address
        .parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

pub async fn put_settings(
    State(state): State<AppState>,
    Json(request): Json<SettingsRequest>,
) -> Response<Body> {
    let timestamp = now();
    let db = state.db.lock().await;
    if let Some(account_id) = request.account_id.as_deref() {
        match db.get_account(account_id) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "travel_account_not_found",
                    "Choose an Envelope mailbox before enabling travel sync.",
                );
            }
            Err(error) => return db_error(error),
        }
    }
    let existing = match db.get_travel_settings() {
        Ok(value) => value,
        Err(error) => return db_error(error),
    };
    let scan_folder = request
        .scan_folder
        .as_deref()
        .or_else(|| existing.as_ref().map(|value| value.scan_folder.as_str()))
        .unwrap_or("INBOX");
    let scan_folder = match normalize_scan_folder(scan_folder) {
        Ok(value) => value,
        Err(message) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "travel_scan_folder_invalid",
                message,
            );
        }
    };
    let settings = TravelSettings {
        account_id: request
            .account_id
            .or_else(|| existing.as_ref().and_then(|v| v.account_id.clone())),
        scan_folder,
        home_timezone: request
            .home_timezone
            .or_else(|| existing.as_ref().map(|v| v.home_timezone.clone()))
            .unwrap_or_else(|| "UTC".into()),
        calendar_name: request
            .calendar_name
            .or_else(|| existing.as_ref().map(|v| v.calendar_name.clone()))
            .unwrap_or_else(|| "Family travel".into()),
        auto_ingest: request
            .auto_ingest
            .or_else(|| existing.as_ref().map(|v| v.auto_ingest))
            .unwrap_or(true),
        default_alert_minutes: request
            .default_alert_minutes
            .or_else(|| existing.as_ref().map(|v| v.default_alert_minutes))
            .unwrap_or(120)
            .clamp(0, 60 * 24 * 30),
        updated_at: timestamp,
    };
    match db.upsert_travel_settings(&settings) {
        Ok(saved) => Json(json!({ "settings": saved })).into_response(),
        Err(error) => db_error(error),
    }
}

#[derive(Debug, Deserialize)]
pub struct NewTripRequest {
    pub title: String,
    pub destination: Option<String>,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub timezone: Option<String>,
    pub status: Option<String>,
    pub notes: Option<String>,
}

pub async fn create_trip(
    State(state): State<AppState>,
    Json(request): Json<NewTripRequest>,
) -> Response<Body> {
    if request.title.trim().is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "travel_title_required",
            "A trip title is required.",
        );
    }
    let timestamp = now();
    let db = state.db.lock().await;
    let timezone = request
        .timezone
        .or_else(|| {
            db.get_travel_settings()
                .ok()
                .flatten()
                .map(|v| v.home_timezone)
        })
        .unwrap_or_else(|| "UTC".into());
    match db.create_trip(&NewTrip {
        id: Uuid::new_v4().to_string(),
        title: request.title.trim().to_string(),
        destination: trim_option(request.destination),
        starts_at: trim_option(request.starts_at),
        ends_at: trim_option(request.ends_at),
        timezone,
        status: request.status.unwrap_or_else(|| "planning".into()),
        notes: trim_option(request.notes),
        now: timestamp,
    }) {
        Ok(trip) => (StatusCode::CREATED, Json(json!({ "trip": trip }))).into_response(),
        Err(error) => db_error(error),
    }
}

#[derive(Debug, Deserialize)]
pub struct BookingRequest {
    pub trip_id: String,
    pub kind: String,
    pub provider: Option<String>,
    pub title: String,
    pub confirmation_code: Option<String>,
    pub status: Option<String>,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub location: Option<String>,
    pub origin: Option<String>,
    pub destination: Option<String>,
    pub service_number: Option<String>,
    pub details: Option<Value>,
}

pub async fn create_booking(
    State(state): State<AppState>,
    Json(request): Json<BookingRequest>,
) -> Response<Body> {
    if request.title.trim().is_empty() || request.kind.trim().is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "travel_booking_fields_required",
            "Booking type and title are required.",
        );
    }
    let BookingRequest {
        trip_id,
        kind,
        provider,
        title,
        confirmation_code,
        status,
        starts_at,
        ends_at,
        location,
        origin,
        destination,
        service_number,
        details,
    } = request;
    let kind = kind.trim().to_string();
    let title = title.trim().to_string();
    let provider = trim_option(provider);
    let confirmation_code = trim_option(confirmation_code);
    let status = trim_option(status).unwrap_or_else(|| "confirmed".into());
    let starts_at = trim_option(starts_at);
    let ends_at = trim_option(ends_at);
    let location = trim_option(location);
    let origin = trim_option(origin);
    let destination = trim_option(destination);
    let service_number = trim_option(service_number);
    let timestamp = now();
    let db = state.db.lock().await;
    let trip = match db.get_trip(&trip_id) {
        Ok(Some(trip)) => trip,
        Ok(None) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "travel_trip_not_found",
                "Trip not found.",
            );
        }
        Err(error) => return db_error(error),
    };
    let reminder_minutes = match db.get_travel_settings() {
        Ok(Some(settings)) => settings.default_alert_minutes,
        Ok(None) => 120,
        Err(error) => return db_error(error),
    };
    let booking_id = Uuid::new_v4().to_string();
    let booking = match db.create_travel_booking(&NewTravelBooking {
        id: booking_id.clone(),
        trip_id: trip_id.clone(),
        receipt_id: None,
        kind: kind.clone(),
        provider: provider.clone(),
        title,
        confirmation_code,
        status: status.clone(),
        starts_at: starts_at.clone(),
        ends_at: ends_at.clone(),
        location: location.clone(),
        details: details.clone(),
        now: timestamp.clone(),
    }) {
        Ok(value) => value,
        Err(error) => return db_error(error),
    };
    let segment = match db.create_travel_segment(&NewTravelSegment {
        id: Uuid::new_v4().to_string(),
        trip_id: trip_id.clone(),
        booking_id: Some(booking_id),
        kind,
        sequence: 0,
        origin,
        destination: destination.clone(),
        carrier: provider,
        service_number,
        departs_at: starts_at.clone(),
        arrives_at: ends_at.clone(),
        status,
        details,
        now: timestamp.clone(),
    }) {
        Ok(value) => value,
        Err(error) => return db_error(error),
    };
    if let Err(error) = expand_trip_bounds_on(
        &db,
        &trip,
        starts_at.as_deref(),
        ends_at.as_deref(),
        destination.as_deref().or(location.as_deref()),
        &timestamp,
    ) {
        return db_error(error);
    }
    if let Err(error) = enqueue_booking_alerts_on(
        &db,
        &trip_id,
        &booking,
        Some(&segment),
        false,
        reminder_minutes,
        &timestamp,
    ) {
        return db_error(error);
    }
    (
        StatusCode::CREATED,
        Json(json!({ "booking": booking_view(booking), "segment": segment })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct TaskRequest {
    pub title: String,
    pub notes: Option<String>,
    pub due_at: Option<String>,
}

pub async fn create_task(
    State(state): State<AppState>,
    Path(trip_id): Path<String>,
    Json(request): Json<TaskRequest>,
) -> Response<Body> {
    if request.title.trim().is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "travel_task_title_required",
            "A task title is required.",
        );
    }
    let db = state.db.lock().await;
    match db.get_trip(&trip_id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "travel_trip_not_found",
                "Trip not found.",
            );
        }
        Err(error) => return db_error(error),
    }
    match db.create_travel_task(&NewTravelTask {
        id: Uuid::new_v4().to_string(),
        trip_id: Some(trip_id),
        title: request.title.trim().to_string(),
        notes: trim_option(request.notes),
        due_at: trim_option(request.due_at),
        now: now(),
    }) {
        Ok(task) => (StatusCode::CREATED, Json(json!({ "task": task }))).into_response(),
        Err(error) => db_error(error),
    }
}

pub async fn toggle_task(
    State(state): State<AppState>,
    Path(task_id): Path<String>,
) -> Response<Body> {
    let db = state.db.lock().await;
    match db.toggle_travel_task(&task_id, &now()) {
        Ok(Some(task)) => Json(json!({ "task": task })).into_response(),
        Ok(None) => api_error(
            StatusCode::NOT_FOUND,
            "travel_task_not_found",
            "Task not found.",
        ),
        Err(error) => db_error(error),
    }
}

pub async fn acknowledge_alert(
    State(state): State<AppState>,
    Path(alert_id): Path<String>,
) -> Response<Body> {
    let db = state.db.lock().await;
    match db.acknowledge_travel_alert(&alert_id, &now()) {
        Ok(Some(alert)) => Json(json!({ "alert": alert })).into_response(),
        Ok(None) => api_error(
            StatusCode::NOT_FOUND,
            "travel_alert_not_found",
            "Alert not found.",
        ),
        Err(error) => db_error(error),
    }
}

#[derive(Debug, Deserialize)]
pub struct ShareRequest {
    pub trip_id: String,
    pub label: Option<String>,
    pub can_edit_tasks: Option<bool>,
    pub expires_at: Option<String>,
}

pub async fn create_share(
    State(state): State<AppState>,
    Json(request): Json<ShareRequest>,
) -> Response<Body> {
    let db = state.db.lock().await;
    match db.get_trip(&request.trip_id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "travel_trip_not_found",
                "Trip not found.",
            );
        }
        Err(error) => return db_error(error),
    }
    let created = match db.create_travel_share_link(&NewTravelShareLink {
        id: Uuid::new_v4().to_string(),
        trip_id: request.trip_id,
        label: trim_option(request.label),
        can_edit_tasks: request.can_edit_tasks.unwrap_or(false),
        expires_at: trim_option(request.expires_at),
        created_at: now(),
    }) {
        Ok(value) => value,
        Err(error) => return db_error(error),
    };
    let token = created.token;
    let calendar_token = created.calendar_token;
    Json(json!({
        "share": created.link,
        "token": token,
        "url": format!("/share/{token}"),
        "calendar_url": format!("/api/public/travel/{calendar_token}/calendar.ics"),
    }))
    .into_response()
}

pub async fn revoke_share(
    State(state): State<AppState>,
    Path(share_id): Path<String>,
) -> Response<Body> {
    let db = state.db.lock().await;
    match db.revoke_travel_share_link(&share_id, &now()) {
        Ok(Some(share)) => Json(json!({ "share": share, "status": "revoked" })).into_response(),
        Ok(None) => api_error(
            StatusCode::NOT_FOUND,
            "travel_share_not_found",
            "Share link not found.",
        ),
        Err(error) => db_error(error),
    }
}

#[derive(Debug, Deserialize)]
pub struct ManualImportRequest {
    pub account_id: Option<String>,
    pub from_addr: String,
    pub subject: String,
    pub received_at: Option<String>,
    pub body_text: String,
    pub html_body: Option<String>,
}

pub async fn import_receipt(
    State(state): State<AppState>,
    Json(request): Json<ManualImportRequest>,
) -> Response<Body> {
    if request.subject.trim().is_empty() || request.body_text.trim().is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "travel_receipt_content_required",
            "Paste both a receipt subject and its text.",
        );
    }
    let account_id = if let Some(id) = request.account_id {
        id
    } else {
        let db = state.db.lock().await;
        db.get_travel_settings()
            .ok()
            .flatten()
            .and_then(|settings| settings.account_id)
            .unwrap_or_else(|| "manual".into())
    };
    let mut parsed = parse_travel_document(
        &request.from_addr,
        &request.subject,
        request.received_at.as_deref(),
        &request.body_text,
        request.html_body.as_deref(),
        None,
    );
    if parsed.decision != "auto_accepted" {
        let known = {
            let db = state.db.lock().await;
            crate::learning::known(
                &db,
                &request.from_addr,
                &request.subject,
                &request.body_text,
            )
            .ok()
            .flatten()
        };
        if let Some(candidate) = known {
            parsed = candidate;
        } else {
            let _ = crate::intelligence::attempt(
                &state,
                &request.from_addr,
                &request.subject,
                &request.body_text,
            )
            .await;
            let db = state.db.lock().await;
            if let Ok(Some(candidate)) = crate::learning::known(
                &db,
                &request.from_addr,
                &request.subject,
                &request.body_text,
            ) {
                parsed = candidate;
            }
        }
    }
    let source = TravelSource {
        account_id,
        folder: "MANUAL".into(),
        uidvalidity: 0,
        uid: manual_uid(),
        message_id: None,
        from_addr: request.from_addr,
        subject: request.subject,
        received_at: request.received_at,
        authenticated_sender_domain: None,
        body_text: request.body_text,
    };
    match persist_parsed_document(&state, source, parsed, true).await {
        Ok(outcome) => (StatusCode::CREATED, Json(outcome)).into_response(),
        Err(PersistTravelError::ReceiptConflict(receipt)) => receipt_state_conflict(receipt),
        Err(PersistTravelError::Message(error)) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "travel_import_failed",
            error,
        ),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub enum NullableStringOverride {
    #[default]
    Omitted,
    Clear,
    Value(String),
}

fn deserialize_nullable_string_override<'de, D>(
    deserializer: D,
) -> Result<NullableStringOverride, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Option::<String>::deserialize(deserializer)? {
        Some(value) => NullableStringOverride::Value(value),
        None => NullableStringOverride::Clear,
    })
}

#[derive(Debug, Default, Deserialize)]
pub struct ReceiptApprovalRequest {
    pub kind: Option<String>,
    pub title: Option<String>,
    #[serde(default, deserialize_with = "deserialize_nullable_string_override")]
    pub provider: NullableStringOverride,
    #[serde(default, deserialize_with = "deserialize_nullable_string_override")]
    pub confirmation_code: NullableStringOverride,
    pub status: Option<String>,
    pub start_at: Option<String>,
    #[serde(default, deserialize_with = "deserialize_nullable_string_override")]
    pub end_at: NullableStringOverride,
    pub timezone: Option<String>,
    #[serde(default, deserialize_with = "deserialize_nullable_string_override")]
    pub origin: NullableStringOverride,
    #[serde(default, deserialize_with = "deserialize_nullable_string_override")]
    pub destination: NullableStringOverride,
    pub address: Option<String>,
    pub service_number: Option<String>,
}

pub async fn approve_receipt(
    State(state): State<AppState>,
    Path(receipt_id): Path<String>,
    Json(request): Json<ReceiptApprovalRequest>,
) -> Response<Body> {
    let receipt = {
        let db = state.db.lock().await;
        match db.get_travel_receipt(&receipt_id) {
            Ok(Some(receipt)) => receipt,
            Ok(None) => {
                return api_error(
                    StatusCode::NOT_FOUND,
                    "travel_receipt_not_found",
                    "Travel receipt not found.",
                );
            }
            Err(error) => return db_error(error),
        }
    };
    if matches!(receipt.status.as_str(), "processed" | "dismissed") {
        return receipt_state_conflict(receipt);
    }
    let Some(mut parsed) = receipt
        .extracted
        .as_ref()
        .and_then(|value| serde_json::from_value::<ParsedTravelDocument>(value.clone()).ok())
    else {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "travel_receipt_parser_data_missing",
            "This receipt has no extracted travel details. Add the booking manually, then dismiss it.",
        );
    };
    apply_receipt_overrides(&mut parsed, request);
    let has_existing_identity = if parsed.start_at.is_none() {
        match find_existing_booking_by_identity(&state, &parsed).await {
            Ok(BookingIdentityMatch::Unique(_, _)) => true,
            Ok(BookingIdentityMatch::None) => false,
            Ok(BookingIdentityMatch::Ambiguous) => {
                return api_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "travel_receipt_identity_ambiguous",
                    "More than one booking uses this confirmation code. Update the booking manually, then dismiss this receipt.",
                );
            }
            Err(error) => {
                return api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "travel_receipt_identity_lookup_failed",
                    error,
                );
            }
        }
    } else {
        false
    };
    if matches!(parsed.kind.as_str(), "unknown" | "receipt")
        || (parsed.start_at.is_none() && !has_existing_identity)
    {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "travel_receipt_details_incomplete",
            "A booking type and either a start time or a matching confirmation code are required. Add the booking manually, then dismiss this receipt.",
        );
    }
    parsed.decision = "review".into();
    parsed.reasons.push("approved_by_owner".into());
    let source = TravelSource {
        account_id: receipt.account_id,
        folder: receipt.folder,
        uidvalidity: receipt.uidvalidity,
        uid: receipt.uid,
        message_id: receipt.message_id,
        from_addr: receipt.from_addr.unwrap_or_default(),
        subject: receipt.subject.unwrap_or_else(|| "Travel receipt".into()),
        received_at: receipt.received_at,
        authenticated_sender_domain: receipt.authenticated_sender_domain,
        body_text: receipt.body_text.unwrap_or_default(),
    };
    match persist_parsed_document(&state, source, parsed, true).await {
        Ok(result) => Json(result).into_response(),
        Err(PersistTravelError::ReceiptConflict(receipt)) => receipt_state_conflict(receipt),
        Err(PersistTravelError::Message(error)) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "travel_receipt_approval_failed",
            error,
        ),
    }
}

pub async fn dismiss_receipt(
    State(state): State<AppState>,
    Path(receipt_id): Path<String>,
) -> Response<Body> {
    let _receipt_operation = state.lock_travel_receipt_operation(&receipt_id).await;
    let dismissed = {
        let db = state.db.lock().await;
        match db.dismiss_travel_receipt(&receipt_id, &now()) {
            Ok(TravelReceiptTerminalTransition::Applied(receipt)) => receipt,
            Ok(TravelReceiptTerminalTransition::Conflict(receipt)) => {
                return receipt_state_conflict(receipt);
            }
            Ok(TravelReceiptTerminalTransition::NotFound) => {
                return api_error(
                    StatusCode::NOT_FOUND,
                    "travel_receipt_not_found",
                    "Travel receipt not found.",
                );
            }
            Err(error) => return db_error(error),
        }
    };
    Json(json!({ "receipt": receipt_view(dismissed), "status": "dismissed" })).into_response()
}

fn apply_receipt_overrides(parsed: &mut ParsedTravelDocument, request: ReceiptApprovalRequest) {
    if let Some(value) = trim_option(request.kind) {
        parsed.kind = value;
    }
    if let Some(value) = trim_option(request.title) {
        parsed.title = value;
    }
    apply_nullable_string_override(&mut parsed.provider, request.provider);
    apply_nullable_string_override(&mut parsed.confirmation_code, request.confirmation_code);
    if let Some(value) = trim_option(request.status) {
        parsed.status = value;
    }
    if request.start_at.is_some() {
        parsed.start_at = trim_option(request.start_at);
    }
    apply_nullable_string_override(&mut parsed.end_at, request.end_at);
    if request.timezone.is_some() {
        parsed.timezone = trim_option(request.timezone);
    }
    apply_nullable_string_override(&mut parsed.origin, request.origin);
    apply_nullable_string_override(&mut parsed.destination, request.destination);
    if request.address.is_some() {
        parsed.address = trim_option(request.address);
    }
    if request.service_number.is_some() {
        parsed.service_number = trim_option(request.service_number);
    }
}

fn apply_nullable_string_override(target: &mut Option<String>, value: NullableStringOverride) {
    match value {
        NullableStringOverride::Omitted => {}
        NullableStringOverride::Clear => *target = None,
        NullableStringOverride::Value(value) => *target = trim_option(Some(value)),
    }
}

fn acknowledge_receipt_review_alerts_on(
    db: &envelope_email_store::Database,
    receipt_id: &str,
    timestamp: &str,
) -> Result<(), String> {
    let dedupe_key = format!("receipt:{receipt_id}:review");
    for alert in db
        .list_travel_alerts(None)
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|alert| alert.dedupe_key == dedupe_key && alert.acknowledged_at.is_none())
    {
        db.acknowledge_travel_alert(&alert.id, timestamp)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[derive(Debug, Deserialize, Default)]
pub struct SyncRequest {
    pub account_id: Option<String>,
    pub folder: Option<String>,
    pub limit: Option<u32>,
    pub full_rescan: Option<bool>,
}

#[derive(Debug, Default, Serialize)]
pub struct SyncResult {
    pub status: String,
    pub folders_scanned: Vec<String>,
    pub messages_examined: usize,
    pub receipts_imported: usize,
    pub bookings_created: usize,
    pub needs_review: usize,
    pub ignored: usize,
    pub errors: Vec<String>,
    pub last_sync_at: String,
}

pub async fn sync(
    State(state): State<AppState>,
    Json(request): Json<SyncRequest>,
) -> Response<Body> {
    match sync_mailbox(&state, request).await {
        Ok(result) => {
            if let Err(error) = trigger_due_alerts(&state).await {
                tracing::warn!("travel alert trigger error: {error}");
            }
            Json(result).into_response()
        }
        Err(message) => api_error(StatusCode::BAD_GATEWAY, "travel_sync_failed", message),
    }
}

/// Periodic, opt-in travel sync. Aggregate page loads never call this; it runs
/// only from the explicit background sweep when settings say `auto_ingest`.
pub async fn background_sync(state: &AppState) -> Result<Option<SyncResult>, String> {
    trigger_due_alerts(state).await?;
    let settings = {
        let db = state.db.lock().await;
        db.get_travel_settings().map_err(|e| e.to_string())?
    };
    let Some(settings) = settings.filter(|value| value.auto_ingest) else {
        return Ok(None);
    };
    let Some(account_id) = settings.account_id else {
        return Ok(None);
    };
    let result = sync_mailbox(
        state,
        SyncRequest {
            account_id: Some(account_id),
            folder: Some(settings.scan_folder),
            limit: Some(DEFAULT_SYNC_LIMIT),
            full_rescan: Some(false),
        },
    )
    .await?;
    if result.status != "complete" {
        return Err(format!(
            "travel mailbox sync was partial: {}",
            result.errors.join("; ")
        ));
    }
    Ok(Some(result))
}

async fn trigger_due_alerts(state: &AppState) -> Result<usize, String> {
    let timestamp = now();
    let db = state.db.lock().await;
    let due = db
        .list_due_travel_alerts(&timestamp, 250)
        .map_err(|error| error.to_string())?;
    let mut triggered = 0;
    for alert in due {
        if db
            .mark_travel_alert_triggered(&alert.id, &timestamp)
            .map_err(|error| error.to_string())?
            .is_some()
        {
            triggered += 1;
        }
    }
    Ok(triggered)
}

#[derive(Debug)]
struct TravelMessageFailure {
    source: TravelMessageSourceKey,
    stage: &'static str,
    error: String,
    summary: Option<MessageSummary>,
}

async fn record_travel_message_failure(
    state: &AppState,
    result: &mut SyncResult,
    folder_blocked: &mut bool,
    failure: TravelMessageFailure,
    timestamp: &str,
) {
    let error = bounded_ingest_error(&failure.error);
    let recorded = {
        let db = state.db.lock().await;
        db.record_travel_ingest_failure(&NewTravelIngestFailure {
            source: failure.source.clone(),
            stage: failure.stage.into(),
            last_error: error.clone(),
            now: timestamp.into(),
        })
    };
    let recorded = match recorded {
        Ok(recorded) => recorded,
        Err(store_error) => {
            *folder_blocked = true;
            result.errors.push(format!(
                "{} UID {}: {error}; could not save retry state: {store_error}",
                failure.source.folder, failure.source.uid
            ));
            return;
        }
    };
    if recorded.quarantined_at.is_some() {
        return;
    }
    if recorded.attempt_count < MAX_MESSAGE_INGEST_ATTEMPTS {
        *folder_blocked = true;
        result.errors.push(format!(
            "{} UID {}: {error} (attempt {}/{MAX_MESSAGE_INGEST_ATTEMPTS})",
            failure.source.folder, failure.source.uid, recorded.attempt_count
        ));
        return;
    }

    let summary = failure.summary.as_ref();
    let receipt_id = Uuid::new_v4().to_string();
    let subject = summary
        .map(|summary| summary.subject.trim())
        .filter(|subject| !subject.is_empty())
        .unwrap_or("Unreadable travel email")
        .to_string();
    let quarantine_reason = format!(
        "Envelope could not read this mailbox message after {} attempts. Add the booking manually or dismiss this receipt.",
        recorded.attempt_count
    );
    let receipt = NewTravelReceipt {
        id: receipt_id,
        account_id: failure.source.account_id.clone(),
        folder: failure.source.folder.clone(),
        uidvalidity: failure.source.uidvalidity,
        uid: failure.source.uid,
        message_id: summary.and_then(|summary| summary.message_id.clone()),
        from_addr: summary
            .map(|summary| summary.from_addr.trim().to_string())
            .filter(|value| !value.is_empty()),
        subject: Some(subject.clone()),
        received_at: summary.and_then(|summary| summary.date.clone()),
        authenticated_sender_domain: None,
        body_text: Some(format!(
            "Envelope could not read this travel-related message. Source: {} UID {}. Last error: {error}",
            failure.source.folder, failure.source.uid
        )),
        extracted: None,
        trip_id: None,
        quarantine_reason: Some(quarantine_reason),
        ingested_at: timestamp.into(),
    };
    let alert = NewTravelAlert {
        id: Uuid::new_v4().to_string(),
        trip_id: None,
        booking_id: None,
        segment_id: None,
        dedupe_key: format!(
            "travel-ingest-failure:{}:{}:{}:{}",
            failure.source.account_id,
            failure.source.folder,
            failure.source.uidvalidity,
            failure.source.uid
        ),
        kind: "receipt_review".into(),
        severity: "attention".into(),
        title: "Travel email could not be read".into(),
        body: Some(format!(
            "{subject} is held in Receipts after {} failed read attempts.",
            recorded.attempt_count
        )),
        scheduled_at: Some(timestamp.into()),
        now: timestamp.into(),
    };
    let outcome = {
        let db = state.db.lock().await;
        db.finalize_travel_ingest_failure_quarantine(&failure.source, &receipt, &alert, timestamp)
    };
    match outcome {
        Ok(ingest) => {
            if ingest.inserted {
                result.receipts_imported += 1;
                result.needs_review += 1;
            }
            result.errors.push(format!(
                "{} UID {}: {error}; held for review after {} attempts",
                failure.source.folder, failure.source.uid, recorded.attempt_count
            ));
        }
        Err(store_error) => {
            *folder_blocked = true;
            result.errors.push(format!(
                "{} UID {}: {error}; could not quarantine after {} attempts: {store_error}",
                failure.source.folder, failure.source.uid, recorded.attempt_count
            ));
        }
    }
}

fn bounded_ingest_error(error: &str) -> String {
    let error = error.trim();
    let mut bounded = error
        .chars()
        .take(MAX_INGEST_ERROR_CHARS)
        .collect::<String>();
    if error.chars().count() > MAX_INGEST_ERROR_CHARS {
        bounded.push('…');
    }
    if bounded.is_empty() {
        "unknown mailbox read error".into()
    } else {
        bounded
    }
}

fn missing_requested_uids(requested_uids: &[u32], fetched_uids: &HashSet<u32>) -> Vec<u32> {
    requested_uids
        .iter()
        .copied()
        .filter(|uid| !fetched_uids.contains(uid))
        .collect()
}

#[derive(Debug)]
struct TravelFetchResult<T> {
    items: Vec<T>,
    isolated_failures: Vec<(u32, String)>,
    sweep_error: Option<String>,
}

fn classify_travel_fetch_result<T>(
    items: Vec<T>,
    failures: Vec<(u32, String)>,
    batch_error: Option<&str>,
) -> TravelFetchResult<T> {
    if failures.is_empty() {
        return TravelFetchResult {
            items,
            isolated_failures: Vec::new(),
            sweep_error: None,
        };
    }

    let broken_session = batch_error.is_some_and(is_broken_mailbox_session_error)
        || failures
            .iter()
            .any(|(_, error)| is_broken_mailbox_session_error(error));
    if broken_session {
        let detail = batch_error
            .or_else(|| failures.first().map(|(_, error)| error.as_str()))
            .unwrap_or("mailbox fetch failed");
        return TravelFetchResult {
            items,
            isolated_failures: Vec::new(),
            sweep_error: Some(format!(
                "mailbox sweep failed; per-message retry counts were not changed: {}",
                bounded_ingest_error(detail)
            )),
        };
    }

    TravelFetchResult {
        items,
        isolated_failures: failures,
        sweep_error: None,
    }
}

fn is_broken_mailbox_session_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    // Transport errors are flattened by the current IMAP layer, so keep this
    // deliberately conservative: explicit connection/session failures plus a
    // small allowlist of provider-wide transient command responses. Message
    // shape errors such as a missing BODY or UID are intentionally absent and
    // remain eligible for the bounded per-message poison budget.
    let explicit_transport_or_provider_outage = [
        "connection",
        "disconnected",
        "broken pipe",
        "timed out",
        "timeout",
        "unexpected eof",
        "end of file",
        "eof",
        "tls",
        "socket",
        "channel closed",
        "connection closed",
        "not authenticated",
        "session",
        "network",
        "server bye",
        " bye",
        "bye ",
        "io error",
        "i/o error",
        "[limit]",
        "temporarily unavailable",
        "service unavailable",
        "[unavailable]",
        "rate limit",
        "throttle",
        "too many requests",
        "server busy",
        "try again",
        "over quota",
        "[serverbug]",
    ]
    .iter()
    .any(|marker| error.contains(marker));
    let scoped_command_server_error = (error.contains("imap") || error.contains("command"))
        && (error.contains("server error") || error.contains("server failure"));
    explicit_transport_or_provider_outage || scoped_command_server_error
}

async fn fetch_travel_summaries_resilient(
    client: &mut envelope_email_transport::ImapClient,
    folder: &str,
    requested_uids: &[u32],
) -> TravelFetchResult<MessageSummary> {
    let uid_set = requested_uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let (mut summaries, retry_uids, batch_error) =
        match envelope_email_transport::imap::fetch_message_summaries_selected_uid_set(
            client, folder, &uid_set,
        )
        .await
        {
            Ok(summaries) => {
                let fetched = summaries
                    .iter()
                    .map(|summary| summary.uid)
                    .collect::<HashSet<_>>();
                let missing = missing_requested_uids(requested_uids, &fetched);
                (summaries, missing, None)
            }
            Err(error) => (Vec::new(), requested_uids.to_vec(), Some(error.to_string())),
        };
    let mut failures = Vec::new();
    for uid in retry_uids {
        match envelope_email_transport::imap::fetch_message_summaries_selected_uid_set(
            client,
            folder,
            &uid.to_string(),
        )
        .await
        {
            Ok(mut retry) => {
                if let Some(position) = retry.iter().position(|summary| summary.uid == uid) {
                    summaries.push(retry.swap_remove(position));
                } else {
                    failures.push((uid, "UID FETCH returned no message summary".into()));
                }
            }
            Err(error) => failures.push((
                uid,
                batch_error
                    .as_deref()
                    .map(|batch| format!("{batch}; individual retry failed: {error}"))
                    .unwrap_or_else(|| error.to_string()),
            )),
        }
    }
    summaries.sort_unstable_by_key(|summary| summary.uid);
    summaries.dedup_by_key(|summary| summary.uid);
    classify_travel_fetch_result(summaries, failures, batch_error.as_deref())
}

async fn fetch_travel_raw_messages_resilient(
    client: &mut envelope_email_transport::ImapClient,
    folder: &str,
    requested_uids: &[u32],
) -> TravelFetchResult<envelope_email_transport::imap::RawMessage> {
    let uid_set = requested_uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let (mut messages, retry_uids, batch_error) =
        match envelope_email_transport::imap::fetch_raw_messages_selected_uid_set(
            client, folder, &uid_set,
        )
        .await
        {
            Ok(messages) => {
                let fetched = messages
                    .iter()
                    .map(|message| message.uid)
                    .collect::<HashSet<_>>();
                let missing = missing_requested_uids(requested_uids, &fetched);
                (messages, missing, None)
            }
            Err(error) => (Vec::new(), requested_uids.to_vec(), Some(error.to_string())),
        };
    let mut failures = Vec::new();
    for uid in retry_uids {
        match envelope_email_transport::imap::fetch_raw_messages_selected_uid_set(
            client,
            folder,
            &uid.to_string(),
        )
        .await
        {
            Ok(mut retry) => {
                if let Some(position) = retry.iter().position(|message| message.uid == uid) {
                    messages.push(retry.swap_remove(position));
                } else {
                    failures.push((uid, "UID FETCH returned no raw message".into()));
                }
            }
            Err(error) => failures.push((
                uid,
                batch_error
                    .as_deref()
                    .map(|batch| format!("{batch}; individual retry failed: {error}"))
                    .unwrap_or_else(|| error.to_string()),
            )),
        }
    }
    messages.sort_unstable_by_key(|message| message.uid);
    messages.dedup_by_key(|message| message.uid);
    classify_travel_fetch_result(messages, failures, batch_error.as_deref())
}

async fn sync_mailbox(state: &AppState, request: SyncRequest) -> Result<SyncResult, String> {
    let settings = {
        let db = state.db.lock().await;
        db.get_travel_settings().map_err(|e| e.to_string())?
    };
    let account_id = request
        .account_id
        .or_else(|| settings.as_ref().and_then(|value| value.account_id.clone()))
        .ok_or_else(|| "Connect a Gmail mailbox in Travel settings first.".to_string())?;
    let preferred_folder = request
        .folder
        .or_else(|| settings.as_ref().map(|value| value.scan_folder.clone()))
        .unwrap_or_else(|| "INBOX".into());
    let preferred_folder = normalize_scan_folder(&preferred_folder)?;
    let limit = request
        .limit
        .unwrap_or(DEFAULT_SYNC_LIMIT)
        .clamp(1, MAX_SYNC_LIMIT);
    let full_rescan = request.full_rescan.unwrap_or(false);

    // Only a receiving service we explicitly trust may supply authentication
    // evidence. A generic IMAP mailbox can contain forged Gmail headers.
    let trust_gmail_auth = {
        let db = state.db.lock().await;
        db.get_account(&account_id)
            .map_err(|e| e.to_string())?
            .is_some_and(|account| account.imap_host.eq_ignore_ascii_case("imap.gmail.com"))
    };

    let (client_arc, _) = state
        .get_or_create_imap(&account_id)
        .await
        .map_err(|error| format!("Mailbox connection failed: {error}"))?;
    let mut client = client_arc.lock().await;
    // Honor the configured scope exactly. Users who want Gmail's archive can
    // choose its All Mail folder explicitly; INBOX must never silently expand
    // into a whole-account scan.
    let folders = vec![preferred_folder];

    let mut result = SyncResult {
        status: "complete".into(),
        last_sync_at: now(),
        ..SyncResult::default()
    };
    let mut seen_message_ids = HashSet::new();
    for folder in folders {
        let selected =
            match envelope_email_transport::imap::examine_folder_info(&mut client, &folder).await {
                Ok(value) => value,
                Err(error) => {
                    result.errors.push(format!("{folder}: {error}"));
                    continue;
                }
            };
        let uidvalidity = i64::from(selected.uidvalidity_key());
        let handled_uids = {
            let db = state.db.lock().await;
            let mut sources = db
                .list_handled_travel_receipt_sources(&account_id, &folder, uidvalidity)
                .map_err(|error| error.to_string())?;
            sources.extend(
                db.list_quarantined_travel_ingest_sources(&account_id, &folder, uidvalidity)
                    .map_err(|error| error.to_string())?,
            );
            sources
                .into_iter()
                .map(|source| source.uid)
                .collect::<HashSet<_>>()
        };
        let cursor = {
            let db = state.db.lock().await;
            if full_rescan {
                db.delete_travel_cursor(&account_id, &folder)
                    .map_err(|error| error.to_string())?;
                db.upsert_travel_cursor(&TravelIngestCursor {
                    account_id: account_id.clone(),
                    folder: folder.clone(),
                    uidvalidity,
                    last_uid: 0,
                    last_message_at: None,
                    updated_at: result.last_sync_at.clone(),
                })
                .map_err(|error| error.to_string())?;
            }
            db.get_travel_cursor(&account_id, &folder)
                .map_err(|error| error.to_string())?
        };
        let epoch_changed = cursor
            .as_ref()
            .is_some_and(|value| value.uidvalidity != uidvalidity);
        let last_uid = if full_rescan || epoch_changed {
            0
        } else {
            cursor.as_ref().map(|value| value.last_uid).unwrap_or(0)
        };
        let available_uids =
            match envelope_email_transport::imap::list_selected_uids(&mut client).await {
                Ok(value) => value,
                Err(error) => {
                    result.errors.push(format!("{folder}: {error}"));
                    continue;
                }
            };
        let bootstrap_recent = !full_rescan && (cursor.is_none() || epoch_changed);
        let backfill_uids = if bootstrap_recent {
            Vec::new()
        } else {
            next_scan_uids(&available_uids, last_uid, limit)
        };
        let next_cursor_uid = backfill_uids
            .last()
            .copied()
            .map(i64::from)
            .unwrap_or(last_uid);
        let scan_uids = if bootstrap_recent {
            latest_scan_uids(&available_uids, limit)
        } else if full_rescan {
            backfill_uids.clone()
        } else {
            // Historical backfill must never hide a newly-arrived delay or
            // cancellation behind years of old mail. Inspect the newest
            // bounded window on every sweep while advancing the durable
            // cursor only through the contiguous oldest-unseen batch.
            merge_scan_uids(
                backfill_uids.clone(),
                latest_scan_uids(&available_uids, limit),
            )
        };
        let scan_uids = scan_uids
            .into_iter()
            .filter(|uid| !handled_uids.contains(&i64::from(*uid)))
            .collect::<Vec<_>>();
        let mut folder_blocked = false;
        let sync_timestamp = result.last_sync_at.clone();
        let mut summaries = Vec::new();
        for batch in scan_uids.chunks(100) {
            let fetched = fetch_travel_summaries_resilient(&mut client, &folder, batch).await;
            summaries.extend(fetched.items);
            if let Some(error) = fetched.sweep_error {
                folder_blocked = true;
                result.errors.push(format!("{folder}: {error}"));
            }
            for (uid, error) in fetched.isolated_failures {
                record_travel_message_failure(
                    state,
                    &mut result,
                    &mut folder_blocked,
                    TravelMessageFailure {
                        source: TravelMessageSourceKey {
                            account_id: account_id.clone(),
                            folder: folder.clone(),
                            uidvalidity,
                            uid: i64::from(uid),
                        },
                        stage: "summary_fetch",
                        error,
                        summary: None,
                    },
                    &sync_timestamp,
                )
                .await;
            }
        }
        result.folders_scanned.push(folder.clone());
        result.messages_examined += summaries.len();
        let mut candidate_uids = Vec::new();
        let mut last_message_at = if epoch_changed {
            None
        } else {
            cursor
                .as_ref()
                .and_then(|value| value.last_message_at.clone())
        };
        for summary in &summaries {
            if summary.uid as i64 <= last_uid {
                continue;
            }
            if last_message_at.as_ref() < summary.date.as_ref() {
                last_message_at = summary.date.clone();
            }
            let classification = classify_candidate(&summary.from_addr, &summary.subject, "");
            if classification.state != "ignored" {
                candidate_uids.push(summary.uid);
            } else {
                result.ignored += 1;
            }
        }

        for batch in candidate_uids.chunks(25) {
            let fetched = fetch_travel_raw_messages_resilient(&mut client, &folder, batch).await;
            if let Some(error) = fetched.sweep_error {
                folder_blocked = true;
                result.errors.push(format!("{folder}: {error}"));
            }
            for (uid, error) in fetched.isolated_failures {
                let summary = summaries.iter().find(|summary| summary.uid == uid).cloned();
                record_travel_message_failure(
                    state,
                    &mut result,
                    &mut folder_blocked,
                    TravelMessageFailure {
                        source: TravelMessageSourceKey {
                            account_id: account_id.clone(),
                            folder: folder.clone(),
                            uidvalidity,
                            uid: i64::from(uid),
                        },
                        stage: "raw_fetch",
                        error,
                        summary,
                    },
                    &sync_timestamp,
                )
                .await;
            }
            for raw in fetched.items {
                if crate::documents::preserve(&raw.rfc822, "message/rfc822").is_err() {
                    folder_blocked = true;
                    result
                        .errors
                        .push("Could not preserve original message; sync paused".into());
                    continue;
                }
                // Gmail's IMAP INTERNALDATE is server-owned provenance. Keep
                // the sender-controlled RFC822 Date only as parser context;
                // receipt ordering across UID epochs must use the mailbox's
                // authoritative arrival timestamp or fail closed when absent.
                let source_received_at =
                    raw.internal_date.as_ref().map(chrono::DateTime::to_rfc3339);
                let source_key = TravelMessageSourceKey {
                    account_id: account_id.clone(),
                    folder: folder.clone(),
                    uidvalidity,
                    uid: i64::from(raw.uid),
                };
                let Some(decoded) = parse_raw_travel_email(&raw.rfc822) else {
                    let summary = summaries
                        .iter()
                        .find(|summary| summary.uid == raw.uid)
                        .cloned();
                    record_travel_message_failure(
                        state,
                        &mut result,
                        &mut folder_blocked,
                        TravelMessageFailure {
                            source: source_key,
                            stage: "decode",
                            error: "malformed RFC822 message".into(),
                            summary,
                        },
                        &sync_timestamp,
                    )
                    .await;
                    continue;
                };
                {
                    let db = state.db.lock().await;
                    if let Err(error) = db.clear_open_travel_ingest_failure(&source_key) {
                        folder_blocked = true;
                        result.errors.push(format!(
                            "{folder} UID {}: could not clear retry state: {error}",
                            raw.uid
                        ));
                        continue;
                    }
                }
                if decoded
                    .message_id
                    .as_ref()
                    .is_some_and(|id| !seen_message_ids.insert(id.to_ascii_lowercase()))
                {
                    continue;
                }
                let parsed = parse_travel_document(
                    &decoded.from_addr,
                    &decoded.subject,
                    decoded.received_at.as_deref(),
                    &decoded.text_body,
                    decoded.html_body.as_deref(),
                    decoded.ics_body.as_deref(),
                );
                if parsed.decision == "ignored" {
                    result.ignored += 1;
                    continue;
                }
                let source = TravelSource {
                    account_id: account_id.clone(),
                    folder: folder.clone(),
                    uidvalidity,
                    uid: i64::from(raw.uid),
                    message_id: decoded.message_id,
                    from_addr: decoded.from_addr,
                    subject: decoded.subject,
                    received_at: source_received_at,
                    authenticated_sender_domain: if trust_gmail_auth {
                        decoded.authenticated_sender_domain
                    } else {
                        None
                    },
                    body_text: decoded.text_body,
                };
                match persist_parsed_document(state, source, parsed, false).await {
                    Ok(outcome) => {
                        if outcome["inserted"].as_bool().unwrap_or(false) {
                            result.receipts_imported += 1;
                        }
                        if outcome["booking_created"].as_bool().unwrap_or(false) {
                            result.bookings_created += 1;
                        }
                        if outcome["needs_review"].as_bool().unwrap_or(false) {
                            result.needs_review += 1;
                        }
                    }
                    Err(error) => {
                        folder_blocked = true;
                        result
                            .errors
                            .push(format!("{folder} UID {}: {error}", raw.uid));
                    }
                }
            }
        }

        // A transient per-message failure keeps the cursor conservative for a
        // retry. Once its bounded retry budget is exhausted, the quarantined
        // receipt and alert become the durable visibility record, so that UID
        // no longer pins the entire historical backfill.
        if !folder_blocked {
            let cursor = TravelIngestCursor {
                account_id: account_id.clone(),
                folder: folder.clone(),
                uidvalidity,
                // A first run checks recent mail immediately, then leaves a
                // zero boundary so later bounded sweeps backfill history from
                // the oldest UID instead of silently abandoning it.
                last_uid: if bootstrap_recent { 0 } else { next_cursor_uid },
                last_message_at,
                updated_at: result.last_sync_at.clone(),
            };
            let db = state.db.lock().await;
            db.upsert_travel_cursor(&cursor)
                .map_err(|error| error.to_string())?;
        }
    }
    let had_errors = !result.errors.is_empty();
    if had_errors {
        result.status = "partial".into();
    }
    drop(client);
    if had_errors {
        state.evict_imap(&account_id).await;
    }
    Ok(result)
}

#[derive(Debug, Clone)]
struct TravelSource {
    account_id: String,
    folder: String,
    uidvalidity: i64,
    uid: i64,
    message_id: Option<String>,
    from_addr: String,
    subject: String,
    received_at: Option<String>,
    authenticated_sender_domain: Option<String>,
    body_text: String,
}

pub async fn ingest_external(
    state: &AppState,
    account: &str,
    external_id: &str,
    raw: &[u8],
    received: Option<String>,
) -> anyhow::Result<()> {
    let decoded = parse_raw_travel_email(raw).ok_or_else(|| anyhow::anyhow!("Unreadable email"))?;
    if classify_candidate(&decoded.from_addr, &decoded.subject, &decoded.text_body).state
        == "ignored"
    {
        return Ok(());
    }
    let parsed = parse_travel_document(
        &decoded.from_addr,
        &decoded.subject,
        decoded.received_at.as_deref(),
        &decoded.text_body,
        decoded.html_body.as_deref(),
        decoded.ics_body.as_deref(),
    );
    let digest = Sha256::digest(external_id.as_bytes());
    let uid = i64::from_be_bytes(digest[..8].try_into().unwrap()) & i64::MAX;
    let source = TravelSource {
        account_id: account.into(),
        folder: format!("GMAIL_API/{external_id}"),
        uidvalidity: 0,
        uid,
        message_id: decoded.message_id,
        from_addr: decoded.from_addr,
        subject: decoded.subject,
        received_at: received,
        authenticated_sender_domain: decoded.authenticated_sender_domain,
        body_text: decoded.text_body,
    };
    persist_parsed_document(state, source, parsed, false)
        .await
        .map_err(|_| anyhow::anyhow!("Receipt requires recovery"))?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutomaticAmendmentDecision {
    Apply,
    Ignore,
    Review(&'static str),
}

#[derive(Debug)]
struct PersistedItinerary {
    trip: envelope_email_store::Trip,
    booking: TravelBooking,
    segment: Option<envelope_email_store::TravelSegment>,
    booking_created: bool,
    amendment_ignored: bool,
}

#[derive(Debug)]
enum BookingIdentityMatch {
    None,
    Unique(envelope_email_store::Trip, TravelBooking),
    Ambiguous,
}

fn automatic_amendment_decision_on(
    db: &envelope_email_store::Database,
    candidate: &TravelReceipt,
    current_booking: &TravelBooking,
    parsed: &ParsedTravelDocument,
    manual: bool,
) -> Result<AutomaticAmendmentDecision, String> {
    if manual {
        return Ok(AutomaticAmendmentDecision::Apply);
    }
    let Some(current_receipt_id) = current_booking.receipt_id.as_deref() else {
        return Ok(AutomaticAmendmentDecision::Review(
            "automatic_amendment_missing_receipt_provenance",
        ));
    };
    let Some(current_receipt) = db
        .get_travel_receipt(current_receipt_id)
        .map_err(|error| error.to_string())?
    else {
        return Ok(AutomaticAmendmentDecision::Review(
            "automatic_amendment_missing_receipt_provenance",
        ));
    };
    if candidate.account_id != current_receipt.account_id {
        return Ok(AutomaticAmendmentDecision::Review(
            "automatic_amendment_cross_account",
        ));
    }

    match compare_travel_receipt_sources(candidate, &current_receipt) {
        TravelReceiptSourceOrder::Older => Ok(AutomaticAmendmentDecision::Ignore),
        TravelReceiptSourceOrder::Same => {
            let changes_status = parsed.status != "unknown"
                && !parsed.status.eq_ignore_ascii_case(&current_booking.status);
            if candidate.id == current_receipt.id && !changes_status {
                // A pending receipt can be replayed after an interrupted older
                // release. Same-source, same-status work is idempotent; a
                // distinct status on the same mailbox source is not.
                Ok(AutomaticAmendmentDecision::Apply)
            } else {
                Ok(AutomaticAmendmentDecision::Ignore)
            }
        }
        TravelReceiptSourceOrder::Incomparable => Ok(AutomaticAmendmentDecision::Review(
            "automatic_amendment_source_incomparable",
        )),
        TravelReceiptSourceOrder::Newer => {
            let candidate_domain = candidate
                .authenticated_sender_domain
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let current_domain = current_receipt
                .authenticated_sender_domain
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let authenticated_domain_matches = candidate_domain
                .zip(current_domain)
                .is_some_and(|(candidate, current)| candidate.eq_ignore_ascii_case(current));
            if !authenticated_domain_matches {
                return Ok(AutomaticAmendmentDecision::Review(
                    "automatic_amendment_sender_not_authenticated",
                ));
            }
            let authenticated_mailbox_matches = normalized_authenticated_sender_mailbox(candidate)
                .zip(normalized_authenticated_sender_mailbox(&current_receipt))
                .is_some_and(|(candidate, current)| candidate == current);
            if !authenticated_mailbox_matches {
                return Ok(AutomaticAmendmentDecision::Review(
                    "automatic_amendment_sender_mailbox_mismatch",
                ));
            }
            if !automatic_booking_identity_is_current_or_compatible(
                db,
                candidate,
                current_booking,
                parsed,
            )? {
                return Ok(AutomaticAmendmentDecision::Review(
                    "automatic_amendment_booking_not_active_or_date_compatible",
                ));
            }
            Ok(AutomaticAmendmentDecision::Apply)
        }
    }
}

/// The stored From header becomes authenticated mailbox provenance only when
/// it is paired with Gmail-aligned DMARC provenance and still parses as one
/// mailbox in that exact authenticated domain.
fn normalized_authenticated_sender_mailbox(receipt: &TravelReceipt) -> Option<String> {
    let authenticated_domain = receipt
        .authenticated_sender_domain
        .as_deref()?
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if authenticated_domain.is_empty() {
        return None;
    }
    let from_addr = receipt.from_addr.as_deref()?.trim();
    if from_addr.is_empty() || from_addr.chars().any(char::is_control) {
        return None;
    }
    let raw = format!("From: {from_addr}\r\n\r\n");
    let message = mail_parser::MessageParser::default().parse(raw.as_bytes())?;
    let address = match message.from()? {
        mail_parser::Address::List(addresses) if addresses.len() == 1 => {
            addresses.first()?.address.as_deref()?
        }
        mail_parser::Address::Group(groups)
            if groups.len() == 1 && groups.first()?.addresses.len() == 1 =>
        {
            groups.first()?.addresses.first()?.address.as_deref()?
        }
        _ => return None,
    };
    let mailbox = envelope_email_store::address_book::normalize_email(address)?;
    mailbox
        .rsplit_once('@')
        .is_some_and(|(_, domain)| domain.eq_ignore_ascii_case(&authenticated_domain))
        .then_some(mailbox)
}

fn automatic_booking_identity_is_current_or_compatible(
    db: &envelope_email_store::Database,
    candidate: &TravelReceipt,
    booking: &TravelBooking,
    parsed: &ParsedTravelDocument,
) -> Result<bool, String> {
    const RECENT_GRACE_DAYS: i64 = 30;

    let trip = db
        .get_trip(&booking.trip_id)
        .map_err(|error| error.to_string())?;
    let existing_start = booking
        .starts_at
        .as_deref()
        .and_then(parse_datetime)
        .or_else(|| {
            trip.as_ref()
                .and_then(|trip| trip.starts_at.as_deref())
                .and_then(parse_datetime)
        });
    let existing_end = booking
        .ends_at
        .as_deref()
        .and_then(parse_datetime)
        .or_else(|| {
            trip.as_ref()
                .and_then(|trip| trip.ends_at.as_deref())
                .and_then(parse_datetime)
        })
        .or(existing_start);
    if parsed.start_at.is_some() {
        return Ok(structured_booking_dates_are_compatible(
            trip.as_ref(),
            booking,
            parsed,
        ));
    }

    let source_time = candidate.received_at.as_deref().and_then(parse_datetime);
    let recent_by_date =
        source_time
            .zip(existing_end)
            .is_some_and(|(source_time, existing_end)| {
                existing_end >= source_time - Duration::days(RECENT_GRACE_DAYS)
            });
    if recent_by_date {
        return Ok(true);
    }

    // A nonterminal status is not dated evidence and can remain stale
    // indefinitely. Missing booking/trip dates therefore fail closed.
    Ok(false)
}

fn structured_booking_dates_are_compatible(
    trip: Option<&envelope_email_store::Trip>,
    booking: &TravelBooking,
    parsed: &ParsedTravelDocument,
) -> bool {
    const COMPATIBLE_DATE_WINDOW_DAYS: i64 = 14;

    let existing_start = booking
        .starts_at
        .as_deref()
        .and_then(parse_datetime)
        .or_else(|| {
            trip.and_then(|trip| trip.starts_at.as_deref())
                .and_then(parse_datetime)
        });
    let existing_end = booking
        .ends_at
        .as_deref()
        .and_then(parse_datetime)
        .or_else(|| {
            trip.and_then(|trip| trip.ends_at.as_deref())
                .and_then(parse_datetime)
        })
        .or(existing_start);
    let candidate_start = parsed.start_at.as_deref().and_then(parse_datetime);
    let candidate_end = parsed
        .end_at
        .as_deref()
        .and_then(parse_datetime)
        .or(candidate_start);
    match (existing_start, existing_end, candidate_start, candidate_end) {
        (Some(existing_start), Some(existing_end), Some(candidate_start), Some(candidate_end)) => {
            let window = Duration::days(COMPATIBLE_DATE_WINDOW_DAYS);
            candidate_start <= existing_end + window && candidate_end >= existing_start - window
        }
        _ => false,
    }
}

fn terminal_receipt_persistence_outcome(
    ingest: &TravelReceiptIngestResult,
    manual: bool,
) -> Result<Option<Value>, PersistTravelError> {
    if !matches!(ingest.receipt.status.as_str(), "processed" | "dismissed") {
        return Ok(None);
    }
    if manual {
        return Err(PersistTravelError::ReceiptConflict(ingest.receipt.clone()));
    }
    Ok(Some(json!({
        "receipt": receipt_view(ingest.receipt.clone()),
        "inserted": false,
        "deduped_by": ingest.deduped_by,
        "booking_created": false,
        "needs_review": false,
    })))
}

async fn persist_parsed_document(
    state: &AppState,
    source: TravelSource,
    parsed: ParsedTravelDocument,
    manual: bool,
) -> Result<Value, PersistTravelError> {
    let mut parsed = parsed;
    if parsed.decision != "auto_accepted" && !manual {
        let learned = {
            let db = state.db.lock().await;
            crate::learning::known(&db, &source.from_addr, &source.subject, &source.body_text)
                .ok()
                .flatten()
        };
        if let Some(candidate) = learned {
            parsed = candidate;
        } else {
            let _ = crate::intelligence::attempt(
                state,
                &source.from_addr,
                &source.subject,
                &source.body_text,
            )
            .await;
            let db = state.db.lock().await;
            if let Ok(Some(candidate)) =
                crate::learning::known(&db, &source.from_addr, &source.subject, &source.body_text)
            {
                parsed = candidate;
            }
        }
    }
    let timestamp = now();
    let extracted = serde_json::to_value(&parsed).map_err(|error| error.to_string())?;
    let needs_review = parsed.decision != "auto_accepted";
    let quarantine_reason = needs_review.then(|| {
        parsed
            .reasons
            .last()
            .cloned()
            .unwrap_or_else(|| "parser_confidence_below_auto_accept".into())
    });
    let receipt_id = Uuid::new_v4().to_string();
    let mut ingest = {
        let db = state.db.lock().await;
        db.ingest_travel_receipt(&NewTravelReceipt {
            id: receipt_id,
            account_id: source.account_id.clone(),
            folder: source.folder,
            uidvalidity: source.uidvalidity,
            uid: source.uid,
            message_id: source.message_id,
            from_addr: Some(source.from_addr),
            subject: Some(source.subject),
            received_at: source.received_at,
            authenticated_sender_domain: source.authenticated_sender_domain,
            body_text: Some(source.body_text),
            extracted: Some(extracted.clone()),
            trip_id: None,
            quarantine_reason,
            ingested_at: timestamp.clone(),
        })
        .map_err(|error| error.to_string())?
    };
    let _receipt_operation = state
        .lock_travel_receipt_operation(&ingest.receipt.id)
        .await;
    ingest.receipt = {
        let db = state.db.lock().await;
        db.get_travel_receipt(&ingest.receipt.id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                PersistTravelError::Message(
                    "receipt disappeared before itinerary persistence".into(),
                )
            })?
    };
    if let Some(outcome) = terminal_receipt_persistence_outcome(&ingest, manual)? {
        return Ok(outcome);
    }

    // Change and cancellation notices commonly repeat only the provider's
    // confirmation code and omit the original dates. Resolve that durable
    // identity before applying the normal "must have a start time" gate.
    // Automatic amendments require newer authenticated provenance; manual
    // owner approval is the explicit override.
    let existing_identity = match find_existing_booking_by_identity(state, &parsed).await? {
        BookingIdentityMatch::None => None,
        BookingIdentityMatch::Unique(trip, booking) => Some((trip, booking)),
        BookingIdentityMatch::Ambiguous => {
            let receipt = defer_receipt_for_review(
                state,
                &ingest.receipt,
                &parsed,
                "automatic_booking_identity_ambiguous",
                &timestamp,
            )
            .await?;
            return Ok(json!({
                "receipt": receipt_view(receipt),
                "inserted": ingest.inserted,
                "deduped_by": ingest.deduped_by,
                "booking_created": false,
                "needs_review": true,
                "parsed": parsed,
            }));
        }
    };
    let amendment_decision = if let Some((_, booking)) = existing_identity.as_ref() {
        let db = state.db.lock().await;
        automatic_amendment_decision_on(&db, &ingest.receipt, booking, &parsed, manual)?
    } else {
        AutomaticAmendmentDecision::Apply
    };
    let trusted_identity_update = existing_identity.as_ref().is_some_and(|(_, booking)| {
        amendment_decision == AutomaticAmendmentDecision::Apply
            && matches!(parsed.status.as_str(), "changed" | "cancelled")
            && parsed.status != booking.status
    });

    if let AutomaticAmendmentDecision::Review(reason) = amendment_decision {
        let receipt =
            defer_receipt_for_review(state, &ingest.receipt, &parsed, reason, &timestamp).await?;
        return Ok(json!({
            "receipt": receipt_view(receipt),
            "inserted": ingest.inserted,
            "deduped_by": ingest.deduped_by,
            "booking_created": false,
            "needs_review": true,
            "parsed": parsed,
        }));
    }

    // A receipt insert is intentionally durable before the atomic itinerary
    // commit. A crash or transient failure leaves this row replayable; no
    // trip, booking, segment, reminder, or alert can escape independently of
    // its final processed transition.
    if amendment_decision != AutomaticAmendmentDecision::Ignore
        && ((parsed.kind == "unknown" && !trusted_identity_update)
            || parsed.kind == "receipt"
            || (parsed.start_at.is_none() && existing_identity.is_none())
            || (needs_review && !manual && !trusted_identity_update))
    {
        let reason = parsed
            .reasons
            .last()
            .map(String::as_str)
            .unwrap_or("parser_confidence_below_auto_accept");
        let receipt =
            defer_receipt_for_review(state, &ingest.receipt, &parsed, reason, &timestamp).await?;
        return Ok(json!({
            "receipt": receipt_view(receipt),
            "inserted": ingest.inserted,
            "deduped_by": ingest.deduped_by,
            "booking_created": false,
            "needs_review": true,
            "parsed": parsed,
        }));
    }

    const REVIEW_REQUIRED_PREFIX: &str = "travel_atomic_review_required:";
    let atomic_result = {
        let db = state.db.lock().await;
        db.process_travel_receipt_atomically(
            &ingest.receipt.id,
            Some(&extracted),
            &timestamp,
            |db| {
                let existing_identity = match find_existing_booking_by_identity_on(db, &parsed)
                    .map_err(envelope_email_store::errors::StoreError::Config)?
                {
                    BookingIdentityMatch::None => None,
                    BookingIdentityMatch::Unique(trip, booking) => Some((trip, booking)),
                    BookingIdentityMatch::Ambiguous => {
                        return Err(envelope_email_store::errors::StoreError::Config(format!(
                            "{REVIEW_REQUIRED_PREFIX}automatic_booking_identity_ambiguous"
                        )));
                    }
                };
                let amendment_decision = if let Some((_, booking)) = existing_identity.as_ref() {
                    automatic_amendment_decision_on(db, &ingest.receipt, booking, &parsed, manual)
                        .map_err(envelope_email_store::errors::StoreError::Config)?
                } else {
                    AutomaticAmendmentDecision::Apply
                };
                if let AutomaticAmendmentDecision::Review(reason) = amendment_decision {
                    return Err(envelope_email_store::errors::StoreError::Config(format!(
                        "{REVIEW_REQUIRED_PREFIX}{reason}"
                    )));
                }
                if let (AutomaticAmendmentDecision::Ignore, Some((trip, booking))) =
                    (amendment_decision, existing_identity.as_ref())
                {
                    let segment = db
                        .list_travel_segments(&trip.id)
                        .map_err(|error| {
                            envelope_email_store::errors::StoreError::Config(error.to_string())
                        })?
                        .into_iter()
                        .find(|segment| segment.booking_id.as_deref() == Some(booking.id.as_str()));
                    acknowledge_receipt_review_alerts_on(db, &ingest.receipt.id, &timestamp)
                        .map_err(envelope_email_store::errors::StoreError::Config)?;
                    return Ok((
                        Some(trip.id.clone()),
                        PersistedItinerary {
                            trip: trip.clone(),
                            booking: booking.clone(),
                            segment,
                            booking_created: false,
                            amendment_ignored: true,
                        },
                    ));
                }

                let settings = db.get_travel_settings()?;
                let timezone = parsed
                    .timezone
                    .clone()
                    .or_else(|| settings.as_ref().map(|value| value.home_timezone.clone()))
                    .unwrap_or_else(|| "UTC".into());
                let trip = if let Some((trip, _)) = existing_identity {
                    trip
                } else {
                    find_or_create_trip_on(db, &parsed, &timezone, &timestamp)
                        .map_err(envelope_email_store::errors::StoreError::Config)?
                };
                let (booking, booking_created, changed) =
                    upsert_booking_on(db, &trip.id, &ingest.receipt.id, &parsed, &timestamp)
                        .map_err(envelope_email_store::errors::StoreError::Config)?;
                let trip = expand_trip_bounds_on(
                    db,
                    &trip,
                    booking.starts_at.as_deref(),
                    booking.ends_at.as_deref(),
                    booking.location.as_deref(),
                    &timestamp,
                )
                .map_err(envelope_email_store::errors::StoreError::Config)?;
                let segment = upsert_segment_on(db, &trip.id, &booking, &parsed, &timestamp)
                    .map_err(envelope_email_store::errors::StoreError::Config)?;
                enqueue_booking_alerts_on(
                    db,
                    &trip.id,
                    &booking,
                    segment.as_ref(),
                    changed,
                    settings
                        .as_ref()
                        .map(|value| value.default_alert_minutes)
                        .unwrap_or(120),
                    &timestamp,
                )
                .map_err(envelope_email_store::errors::StoreError::Config)?;
                acknowledge_receipt_review_alerts_on(db, &ingest.receipt.id, &timestamp)
                    .map_err(envelope_email_store::errors::StoreError::Config)?;
                Ok((
                    Some(trip.id.clone()),
                    PersistedItinerary {
                        trip,
                        booking,
                        segment,
                        booking_created,
                        amendment_ignored: false,
                    },
                ))
            },
        )
    };
    let (receipt, persisted) = match atomic_result {
        Ok(TravelReceiptAtomicTransition::Applied { receipt, value }) => (receipt, value),
        Ok(TravelReceiptAtomicTransition::Conflict(receipt)) => {
            return Err(PersistTravelError::ReceiptConflict(receipt));
        }
        Ok(TravelReceiptAtomicTransition::NotFound) => {
            return Err(PersistTravelError::Message(
                "receipt disappeared before processing".into(),
            ));
        }
        Err(envelope_email_store::errors::StoreError::Config(message))
            if message.starts_with(REVIEW_REQUIRED_PREFIX) =>
        {
            let reason = message
                .strip_prefix(REVIEW_REQUIRED_PREFIX)
                .unwrap_or("automatic_amendment_requires_review");
            let receipt =
                defer_receipt_for_review(state, &ingest.receipt, &parsed, reason, &timestamp)
                    .await?;
            return Ok(json!({
                "receipt": receipt_view(receipt),
                "inserted": ingest.inserted,
                "deduped_by": ingest.deduped_by,
                "booking_created": false,
                "needs_review": true,
                "parsed": parsed,
            }));
        }
        Err(error) => return Err(PersistTravelError::Message(error.to_string())),
    };
    Ok(json!({
        "receipt": receipt_view(receipt),
        "trip": persisted.trip,
        "booking": booking_view(persisted.booking),
        "segment": persisted.segment,
        "inserted": ingest.inserted,
        "booking_created": persisted.booking_created,
        "amendment_ignored": persisted.amendment_ignored,
        "needs_review": !manual && !persisted.amendment_ignored && needs_review && !trusted_identity_update,
        "parsed": parsed,
    }))
}

async fn defer_receipt_for_review(
    state: &AppState,
    receipt: &TravelReceipt,
    parsed: &ParsedTravelDocument,
    reason: &str,
    timestamp: &str,
) -> Result<TravelReceipt, PersistTravelError> {
    let db = state.db.lock().await;
    let alert = review_alert_input(receipt, parsed, timestamp);
    match db
        .quarantine_travel_receipt_with_review_alert(&receipt.id, reason, &alert, timestamp)
        .map_err(|error| error.to_string())?
    {
        TravelReceiptReviewTransition::Applied(receipt) => Ok(receipt),
        TravelReceiptReviewTransition::Conflict(receipt) => {
            Err(PersistTravelError::ReceiptConflict(receipt))
        }
        TravelReceiptReviewTransition::NotFound => Err(PersistTravelError::Message(
            "receipt disappeared while deferring it for review".into(),
        )),
    }
}

async fn find_existing_booking_by_identity(
    state: &AppState,
    parsed: &ParsedTravelDocument,
) -> Result<BookingIdentityMatch, String> {
    let db = state.db.lock().await;
    find_existing_booking_by_identity_on(&db, parsed)
}

fn find_existing_booking_by_identity_on(
    db: &envelope_email_store::Database,
    parsed: &ParsedTravelDocument,
) -> Result<BookingIdentityMatch, String> {
    let Some(code) = parsed
        .confirmation_code
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(BookingIdentityMatch::None);
    };
    let mut matches = Vec::new();
    for trip in db.list_trips().map_err(|error| error.to_string())? {
        for booking in db
            .list_travel_bookings(&trip.id)
            .map_err(|error| error.to_string())?
        {
            let code_matches = booking
                .confirmation_code
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case(code));
            let provider_matches = parsed.provider.is_none()
                || providers_match(booking.provider.as_deref(), parsed.provider.as_deref());
            let dates_match = parsed.start_at.is_none()
                || structured_booking_dates_are_compatible(Some(&trip), &booking, parsed);
            if code_matches && provider_matches && dates_match {
                matches.push((trip.clone(), booking));
            }
        }
    }
    Ok(match matches.len() {
        0 => BookingIdentityMatch::None,
        1 => {
            let (trip, booking) = matches.remove(0);
            BookingIdentityMatch::Unique(trip, booking)
        }
        _ => BookingIdentityMatch::Ambiguous,
    })
}

#[cfg(test)]
async fn find_or_create_trip(
    state: &AppState,
    parsed: &ParsedTravelDocument,
    timezone: &str,
    timestamp: &str,
) -> Result<envelope_email_store::Trip, String> {
    let db = state.db.lock().await;
    find_or_create_trip_on(&db, parsed, timezone, timestamp)
}

fn find_or_create_trip_on(
    db: &envelope_email_store::Database,
    parsed: &ParsedTravelDocument,
    timezone: &str,
    timestamp: &str,
) -> Result<envelope_email_store::Trip, String> {
    let trips = db.list_trips().map_err(|error| error.to_string())?;
    if let Some(existing) = select_matching_trip(&trips, parsed) {
        return expand_trip_bounds_on(
            &db,
            existing,
            parsed.start_at.as_deref(),
            parsed.end_at.as_deref(),
            parsed.destination.as_deref().or(parsed.address.as_deref()),
            timestamp,
        );
    }
    let destination = parsed
        .destination
        .clone()
        .or_else(|| parsed.address.clone());
    let title = destination
        .as_ref()
        .map(|value| format!("Trip to {value}"))
        .unwrap_or_else(|| parsed.title.clone());
    db.create_trip(&NewTrip {
        id: Uuid::new_v4().to_string(),
        title,
        destination,
        starts_at: parsed.start_at.clone(),
        ends_at: parsed.end_at.clone().or_else(|| parsed.start_at.clone()),
        timezone: timezone.to_string(),
        status: "upcoming".into(),
        notes: None,
        now: timestamp.to_string(),
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
fn trip_matches(trip: &envelope_email_store::Trip, parsed: &ParsedTravelDocument) -> bool {
    trip_match_score(trip, parsed).is_some()
}

fn select_matching_trip<'a>(
    trips: &'a [envelope_email_store::Trip],
    parsed: &ParsedTravelDocument,
) -> Option<&'a envelope_email_store::Trip> {
    let mut best = None;
    let mut best_score = 0;
    let mut ambiguous = false;
    for trip in trips {
        let Some(score) = trip_match_score(trip, parsed) else {
            continue;
        };
        if score > best_score {
            best = Some(trip);
            best_score = score;
            ambiguous = false;
        } else if score == best_score {
            ambiguous = true;
        }
    }
    (!ambiguous).then_some(best).flatten()
}

fn trip_match_score(
    trip: &envelope_email_store::Trip,
    parsed: &ParsedTravelDocument,
) -> Option<u8> {
    let Some(candidate) = parsed.start_at.as_deref().and_then(parse_datetime) else {
        return None;
    };
    let start = trip.starts_at.as_deref().and_then(parse_datetime);
    let end = trip.ends_at.as_deref().and_then(parse_datetime).or(start);
    let (Some(start), Some(end)) = (start, end) else {
        return None;
    };
    if candidate < start - Duration::hours(36) || candidate > end + Duration::hours(36) {
        return None;
    }

    match trip_place_relation(trip, parsed) {
        PlaceRelation::Match => Some(3),
        PlaceRelation::Unknown => Some(1),
        PlaceRelation::Mismatch => {
            let candidate_end = parsed.end_at.as_deref().and_then(parse_datetime);
            let continues_after =
                candidate >= end - Duration::hours(12) && candidate <= end + Duration::hours(36);
            let continues_before = candidate_end.is_some_and(|candidate_end| {
                candidate_end >= start - Duration::hours(36)
                    && candidate_end <= start + Duration::hours(12)
            });
            (continues_after || continues_before).then_some(2)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaceRelation {
    Match,
    Mismatch,
    Unknown,
}

fn trip_place_relation(
    trip: &envelope_email_store::Trip,
    parsed: &ParsedTravelDocument,
) -> PlaceRelation {
    let Some(trip_place) = trip.destination.as_deref().map(normalize_place) else {
        return PlaceRelation::Unknown;
    };
    if trip_place.is_empty() {
        return PlaceRelation::Unknown;
    }
    let parsed_places = [
        parsed.origin.as_deref(),
        parsed.destination.as_deref(),
        parsed.address.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(normalize_place)
    .filter(|value| !value.is_empty())
    .collect::<Vec<_>>();
    if parsed_places.is_empty() {
        return PlaceRelation::Unknown;
    }
    if parsed_places
        .iter()
        .any(|candidate| places_match(&trip_place, candidate))
    {
        return PlaceRelation::Match;
    }
    if parsed_places.iter().any(|value| place_is_strong(value)) {
        PlaceRelation::Mismatch
    } else {
        // Airport/station codes are deliberately weak evidence. Treating a
        // city and its three-letter code as incompatible would split ordinary
        // outbound, return, and multi-leg itineraries.
        PlaceRelation::Unknown
    }
}

fn normalize_place(value: &str) -> String {
    let mut normalized = String::new();
    let mut pending_space = false;
    for character in value.trim().chars() {
        if character.is_alphanumeric() {
            if pending_space && !normalized.is_empty() {
                normalized.push(' ');
            }
            normalized.extend(character.to_lowercase());
            pending_space = false;
        } else {
            pending_space = true;
        }
    }
    normalized
}

fn places_match(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    left.split_whitespace().any(|left_token| {
        meaningful_place_token(left_token)
            && right
                .split_whitespace()
                .any(|right_token| left_token == right_token)
    })
}

fn place_is_strong(value: &str) -> bool {
    value.split_whitespace().any(meaningful_place_token)
}

fn meaningful_place_token(value: &str) -> bool {
    value.chars().count() >= 4
        && !matches!(
            value,
            "airport"
                | "station"
                | "terminal"
                | "hotel"
                | "resort"
                | "international"
                | "city"
                | "downtown"
                | "center"
                | "centre"
                | "plaza"
                | "place"
                | "square"
                | "boulevard"
                | "calle"
                | "avenida"
                | "suites"
                | "grand"
                | "street"
                | "avenue"
                | "road"
                | "drive"
        )
}

fn earlier_timestamp(current: Option<&str>, candidate: Option<&str>) -> Option<String> {
    match (current, candidate) {
        (None, None) => None,
        (Some(value), None) | (None, Some(value)) => Some(value.to_string()),
        (Some(current), Some(candidate)) => {
            match (parse_datetime(current), parse_datetime(candidate)) {
                (Some(left), Some(right)) if right < left => Some(candidate.to_string()),
                _ => Some(current.to_string()),
            }
        }
    }
}

fn later_timestamp(current: Option<&str>, candidate: Option<&str>) -> Option<String> {
    match (current, candidate) {
        (None, None) => None,
        (Some(value), None) | (None, Some(value)) => Some(value.to_string()),
        (Some(current), Some(candidate)) => {
            match (parse_datetime(current), parse_datetime(candidate)) {
                (Some(left), Some(right)) if right > left => Some(candidate.to_string()),
                _ => Some(current.to_string()),
            }
        }
    }
}

fn expand_trip_bounds_on(
    db: &envelope_email_store::Database,
    trip: &envelope_email_store::Trip,
    starts_at: Option<&str>,
    ends_at: Option<&str>,
    destination: Option<&str>,
    timestamp: &str,
) -> Result<envelope_email_store::Trip, String> {
    let expanded_start = earlier_timestamp(trip.starts_at.as_deref(), starts_at);
    let candidate_end = ends_at.or(starts_at);
    let expanded_end = later_timestamp(trip.ends_at.as_deref(), candidate_end);
    let destination = trip
        .destination
        .clone()
        .or_else(|| destination.map(str::to_string));
    if expanded_start == trip.starts_at
        && expanded_end == trip.ends_at
        && destination == trip.destination
    {
        return Ok(trip.clone());
    }
    db.update_trip(
        &trip.id,
        &TripUpdate {
            title: trip.title.clone(),
            destination,
            starts_at: expanded_start,
            ends_at: expanded_end,
            timezone: trip.timezone.clone(),
            status: trip.status.clone(),
            notes: trip.notes.clone(),
            updated_at: timestamp.to_string(),
        },
    )
    .map_err(|error| error.to_string())?
    .ok_or_else(|| "trip disappeared during bounds update".to_string())
}

fn upsert_booking_on(
    db: &envelope_email_store::Database,
    trip_id: &str,
    receipt_id: &str,
    parsed: &ParsedTravelDocument,
    timestamp: &str,
) -> Result<(TravelBooking, bool, bool), String> {
    let bookings = db
        .list_travel_bookings(trip_id)
        .map_err(|error| error.to_string())?;
    let trip = parsed
        .start_at
        .as_ref()
        .map(|_| db.get_trip(trip_id).map_err(|error| error.to_string()))
        .transpose()?
        .flatten();
    let existing = bookings
        .iter()
        .find(|booking| booking.receipt_id.as_deref() == Some(receipt_id))
        .or_else(|| {
            parsed.confirmation_code.as_deref().and_then(|code| {
                bookings.iter().find(|booking| {
                    booking
                        .confirmation_code
                        .as_deref()
                        .is_some_and(|value| value.eq_ignore_ascii_case(code))
                        && providers_match(booking.provider.as_deref(), parsed.provider.as_deref())
                        && (parsed.start_at.is_none()
                            || structured_booking_dates_are_compatible(
                                trip.as_ref(),
                                booking,
                                parsed,
                            ))
                })
            })
        });
    if let Some(existing) = existing {
        let status_only = parsed.start_at.is_none();
        let kind = if parsed.kind == "unknown" {
            existing.kind.clone()
        } else {
            parsed.kind.clone()
        };
        let provider = parsed
            .provider
            .clone()
            .or_else(|| existing.provider.clone());
        let title = if status_only || parsed.title.trim().is_empty() {
            existing.title.clone()
        } else {
            parsed.title.clone()
        };
        let confirmation_code = parsed
            .confirmation_code
            .clone()
            .or_else(|| existing.confirmation_code.clone());
        let status = if parsed.status == "unknown" {
            existing.status.clone()
        } else {
            parsed.status.clone()
        };
        let starts_at = parsed
            .start_at
            .clone()
            .or_else(|| existing.starts_at.clone());
        let ends_at = parsed.end_at.clone().or_else(|| existing.ends_at.clone());
        let location = parsed
            .address
            .clone()
            .or_else(|| parsed.destination.clone())
            .or_else(|| existing.location.clone());
        let changed = existing.kind != kind
            || existing.provider != provider
            || existing.title != title
            || existing.confirmation_code != confirmation_code
            || existing.starts_at != starts_at
            || existing.ends_at != ends_at
            || existing.status != status
            || existing.location != location;
        let updated = db
            .update_travel_booking(
                &existing.id,
                &TravelBookingUpdate {
                    receipt_id: Some(receipt_id.into()),
                    kind,
                    provider,
                    title,
                    confirmation_code,
                    status,
                    starts_at,
                    ends_at,
                    location,
                    details: Some(serde_json::to_value(parsed).map_err(|e| e.to_string())?),
                    updated_at: timestamp.into(),
                },
            )
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "booking disappeared during update".to_string())?;
        return Ok((updated, false, changed));
    }
    let created = db
        .create_travel_booking(&NewTravelBooking {
            id: Uuid::new_v4().to_string(),
            trip_id: trip_id.into(),
            receipt_id: Some(receipt_id.into()),
            kind: parsed.kind.clone(),
            provider: parsed.provider.clone(),
            title: parsed.title.clone(),
            confirmation_code: parsed.confirmation_code.clone(),
            status: parsed.status.clone(),
            starts_at: parsed.start_at.clone(),
            ends_at: parsed.end_at.clone(),
            location: parsed
                .address
                .clone()
                .or_else(|| parsed.destination.clone()),
            details: Some(serde_json::to_value(parsed).map_err(|e| e.to_string())?),
            now: timestamp.into(),
        })
        .map_err(|error| error.to_string())?;
    Ok((created, true, false))
}

fn upsert_segment_on(
    db: &envelope_email_store::Database,
    trip_id: &str,
    booking: &TravelBooking,
    parsed: &ParsedTravelDocument,
    timestamp: &str,
) -> Result<Option<envelope_email_store::TravelSegment>, String> {
    let existing = db
        .list_travel_segments(trip_id)
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|segment| segment.booking_id.as_deref() == Some(&booking.id));
    let details = Some(serde_json::to_value(parsed).map_err(|e| e.to_string())?);
    if let Some(existing) = existing {
        let kind = if parsed.kind == "unknown" {
            existing.kind.clone()
        } else {
            parsed.kind.clone()
        };
        return db
            .update_travel_segment(
                &existing.id,
                &TravelSegmentUpdate {
                    booking_id: Some(booking.id.clone()),
                    kind,
                    sequence: existing.sequence,
                    origin: parsed.origin.clone().or(existing.origin.clone()),
                    destination: parsed.destination.clone().or(existing.destination.clone()),
                    carrier: parsed.provider.clone().or(existing.carrier.clone()),
                    service_number: parsed
                        .service_number
                        .clone()
                        .or(existing.service_number.clone()),
                    departs_at: parsed.start_at.clone().or(existing.departs_at.clone()),
                    arrives_at: parsed.end_at.clone().or(existing.arrives_at.clone()),
                    status: if parsed.status == "unknown" {
                        existing.status.clone()
                    } else {
                        parsed.status.clone()
                    },
                    details,
                    updated_at: timestamp.into(),
                },
            )
            .map_err(|error| error.to_string());
    }
    if parsed.start_at.is_none() {
        return Ok(None);
    }
    db.create_travel_segment(&NewTravelSegment {
        id: Uuid::new_v4().to_string(),
        trip_id: trip_id.into(),
        booking_id: Some(booking.id.clone()),
        kind: parsed.kind.clone(),
        sequence: 0,
        origin: parsed.origin.clone(),
        destination: parsed.destination.clone(),
        carrier: parsed.provider.clone(),
        service_number: parsed.service_number.clone(),
        departs_at: parsed.start_at.clone(),
        arrives_at: parsed.end_at.clone(),
        status: parsed.status.clone(),
        details,
        now: timestamp.into(),
    })
    .map(Some)
    .map_err(|error| error.to_string())
}

fn review_alert_input(
    receipt: &TravelReceipt,
    parsed: &ParsedTravelDocument,
    timestamp: &str,
) -> NewTravelAlert {
    NewTravelAlert {
        id: Uuid::new_v4().to_string(),
        trip_id: receipt.trip_id.clone(),
        booking_id: None,
        segment_id: None,
        dedupe_key: format!("receipt:{}:review", receipt.id),
        kind: "receipt_review".into(),
        severity: "attention".into(),
        title: "Travel receipt needs review".into(),
        body: Some(format!(
            "{} could not be imported confidently.",
            parsed.provider.as_deref().unwrap_or("A travel message")
        )),
        scheduled_at: Some(timestamp.into()),
        now: timestamp.into(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn enqueue_booking_alerts_on(
    db: &envelope_email_store::Database,
    trip_id: &str,
    booking: &TravelBooking,
    segment: Option<&envelope_email_store::TravelSegment>,
    changed: bool,
    reminder_minutes: i64,
    timestamp: &str,
) -> Result<(), String> {
    let segment_id = segment.map(|value| value.id.clone());
    let cancelled = matches!(
        booking.status.to_ascii_lowercase().as_str(),
        "cancelled" | "canceled"
    );
    let reminder = (!cancelled)
        .then(|| booking.starts_at.as_deref().and_then(parse_datetime))
        .flatten()
        .map(|start| {
            let due = start - Duration::minutes(reminder_minutes.max(0));
            // A departure offset is not evidence that airline check-in opens.
            let kind = "upcoming";
            let dedupe_key = format!(
                "booking:{}:reminder:{kind}:{}",
                booking.id,
                due.to_rfc3339()
            );
            (kind, due, dedupe_key)
        });
    db.delete_travel_booking_reminders_except(
        &booking.id,
        reminder
            .as_ref()
            .map(|(_, _, dedupe_key)| dedupe_key.as_str()),
    )
    .map_err(|error| error.to_string())?;

    if cancelled || changed {
        let kind = if cancelled {
            "booking_cancelled"
        } else {
            "booking_changed"
        };
        db.enqueue_travel_alert(&NewTravelAlert {
            id: Uuid::new_v4().to_string(),
            trip_id: Some(trip_id.into()),
            booking_id: Some(booking.id.clone()),
            segment_id: segment_id.clone(),
            dedupe_key: format!("booking:{}:{kind}:{}", booking.id, booking.updated_at),
            kind: kind.into(),
            severity: if cancelled { "critical" } else { "attention" }.into(),
            title: if cancelled {
                format!("Cancelled: {}", booking.title)
            } else {
                format!("Updated: {}", booking.title)
            },
            body: Some("Review the latest receipt and coordinate any family changes.".into()),
            scheduled_at: Some(timestamp.into()),
            now: timestamp.into(),
        })
        .map_err(|error| error.to_string())?;
    } else {
        db.enqueue_travel_alert(&NewTravelAlert {
            id: Uuid::new_v4().to_string(),
            trip_id: Some(trip_id.into()),
            booking_id: Some(booking.id.clone()),
            segment_id: segment_id.clone(),
            dedupe_key: format!("booking:{}:imported", booking.id),
            kind: "booking_imported".into(),
            severity: "info".into(),
            title: format!("Added: {}", booking.title),
            body: None,
            scheduled_at: Some(timestamp.into()),
            now: timestamp.into(),
        })
        .map_err(|error| error.to_string())?;
    }
    if let Some((kind, due, dedupe_key)) = reminder {
        db.enqueue_travel_alert(&NewTravelAlert {
            id: Uuid::new_v4().to_string(),
            trip_id: Some(trip_id.into()),
            booking_id: Some(booking.id.clone()),
            segment_id,
            dedupe_key,
            kind: kind.into(),
            severity: "attention".into(),
            title: if kind == "check_in" {
                format!("Check in for {}", booking.title)
            } else {
                format!("Coming up: {}", booking.title)
            },
            body: booking
                .location
                .as_ref()
                .map(|value| format!("Location: {value}")),
            scheduled_at: Some(due.to_rfc3339()),
            now: timestamp.into(),
        })
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn providers_match(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
        _ => true,
    }
}

fn parse_datetime(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
        .or_else(|| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S")
                .ok()
                .map(|value| value.and_utc())
        })
}

fn trim_option(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn normalize_scan_folder(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(
            "Enter an exact Gmail folder name between 1 and 512 characters, without control characters."
                .into(),
        );
    }
    Ok(value.to_string())
}

fn next_scan_uids(available_uids: &[u32], last_uid: i64, limit: u32) -> Vec<u32> {
    let mut uids = available_uids
        .iter()
        .copied()
        .filter(|uid| i64::from(*uid) > last_uid)
        .collect::<Vec<_>>();
    uids.sort_unstable();
    uids.truncate(limit as usize);
    uids
}

fn latest_scan_uids(available_uids: &[u32], limit: u32) -> Vec<u32> {
    let mut uids = available_uids.to_vec();
    uids.sort_unstable();
    let keep_from = uids.len().saturating_sub(limit as usize);
    uids.split_off(keep_from)
}

fn merge_scan_uids(mut backfill: Vec<u32>, recent: Vec<u32>) -> Vec<u32> {
    backfill.extend(recent);
    backfill.sort_unstable();
    backfill.dedup();
    backfill
}

fn manual_uid() -> i64 {
    let bytes = Uuid::new_v4().into_bytes();
    i64::from_be_bytes(bytes[..8].try_into().expect("uuid has eight prefix bytes")) & i64::MAX
}

// ── Public capability-token endpoints ───────────────────────────────

pub async fn public_trip(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Response<Body> {
    let db = state.db.lock().await;
    let mut response = match db.public_trip_view(&token, &now()) {
        Ok(Some(view)) => {
            let calendar_token = envelope_email_store::derive_travel_calendar_token(&token);
            let mut value = serde_json::to_value(view).unwrap_or_else(|_| json!({}));
            if let Value::Object(fields) = &mut value {
                fields.insert(
                    "calendar_url".into(),
                    Value::String(format!("/api/public/travel/{calendar_token}/calendar.ics")),
                );
            }
            Json(value).into_response()
        }
        Ok(None) => api_error(
            StatusCode::GONE,
            "travel_share_unavailable",
            "This travel share has expired or been revoked.",
        ),
        Err(error) => db_error(error),
    };
    // Capability URLs are revocable and may expose a family's live itinerary.
    // Never let a browser reuse a prior success or `410 Gone` after the owner
    // changes or replaces the share.
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

pub async fn public_toggle_task(
    State(state): State<AppState>,
    Path((token, task_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    if !headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"))
    {
        return api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "travel_share_json_required",
            "Shared task updates require a JSON request.",
        );
    }
    let timestamp = now();
    let db = state.db.lock().await;
    let share = match db.lookup_travel_share_link(&token, &timestamp) {
        Ok(Some(value)) if value.can_edit_tasks => value,
        Ok(Some(_)) => {
            return api_error(
                StatusCode::FORBIDDEN,
                "travel_share_read_only",
                "This family link is view-only.",
            );
        }
        Ok(None) => {
            return api_error(
                StatusCode::GONE,
                "travel_share_unavailable",
                "This travel share has expired or been revoked.",
            );
        }
        Err(error) => return db_error(error),
    };
    match db.get_travel_task(&task_id) {
        Ok(Some(task)) if task.trip_id.as_deref() == Some(&share.trip_id) => {}
        Ok(_) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "travel_task_not_found",
                "Task not found for this shared trip.",
            );
        }
        Err(error) => return db_error(error),
    }
    let mut response = match db.toggle_travel_task(&task_id, &timestamp) {
        Ok(Some(task)) => Json(json!({
            "task": {
                "id": task.id,
                "title": task.title,
                "due_at": task.due_at,
                "completed_at": task.completed_at,
            }
        }))
        .into_response(),
        Ok(None) => api_error(
            StatusCode::NOT_FOUND,
            "travel_task_not_found",
            "Task not found.",
        ),
        Err(error) => db_error(error),
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

pub async fn public_calendar(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Response<Body> {
    let db = state.db.lock().await;
    let view = match db.public_trip_calendar_view(&token, &now()) {
        Ok(Some(value)) => value,
        Ok(None) => {
            return api_error(
                StatusCode::GONE,
                "travel_share_unavailable",
                "This calendar feed has expired or been revoked.",
            );
        }
        Err(error) => return db_error(error),
    };
    let mut events: Vec<TravelCalendarEvent> = view
        .segments
        .iter()
        .filter_map(|segment| {
            let start_at = segment.departs_at.clone()?;
            let route = match (&segment.origin, &segment.destination) {
                (Some(origin), Some(destination)) => format!("{origin} to {destination}"),
                _ => segment
                    .destination
                    .clone()
                    .unwrap_or_else(|| title_case(&segment.kind)),
            };
            let service = segment
                .service_number
                .as_ref()
                .map(|number| format!(" · {number}"))
                .unwrap_or_default();
            Some(TravelCalendarEvent {
                id: segment.id.clone(),
                summary: format!("{}{service}: {route}", title_case(&segment.kind)),
                description: segment.carrier.clone(),
                location: segment.origin.clone(),
                start_at,
                end_at: segment.arrives_at.clone(),
                timezone: Some(view.trip.timezone.clone()),
                status: segment.status.clone(),
                sequence: calendar_sequence(&segment.updated_at),
                updated_at: Some(segment.updated_at.clone()),
                url: None,
                alarm_minutes: None,
            })
        })
        .collect();
    let segment_booking_ids: HashSet<&str> = view
        .segments
        .iter()
        .filter_map(|segment| segment.booking_id.as_deref())
        .collect();
    events.extend(view.bookings.iter().filter_map(|booking| {
        if segment_booking_ids.contains(booking.id.as_str()) {
            return None;
        }
        Some(TravelCalendarEvent {
            id: booking.id.clone(),
            summary: booking.title.clone(),
            description: booking.provider.clone(),
            location: booking.location.clone(),
            start_at: booking.starts_at.clone()?,
            end_at: booking.ends_at.clone(),
            timezone: Some(view.trip.timezone.clone()),
            status: booking.status.clone(),
            sequence: calendar_sequence(&booking.updated_at),
            updated_at: Some(booking.updated_at.clone()),
            url: None,
            alarm_minutes: None,
        })
    }));
    let calendar = render_travel_calendar(&view.trip.title, &events);
    let etag = format!("\"{:x}\"", Sha256::digest(calendar.as_bytes()));
    let mut response = Response::new(Body::from(calendar));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/calendar; charset=utf-8"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("inline; filename=family-travel.ics"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=300, must-revalidate"),
    );
    if let Ok(value) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, value);
    }
    response
}

fn calendar_sequence(updated_at: &str) -> u32 {
    DateTime::parse_from_rfc3339(updated_at)
        .ok()
        .and_then(|value| u32::try_from(value.timestamp().max(0)).ok())
        .unwrap_or(0)
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_views_mask_confirmation_codes() {
        let value = booking_view(TravelBooking {
            id: "b1".into(),
            trip_id: "t1".into(),
            receipt_id: None,
            kind: "flight".into(),
            provider: Some("Air Test".into()),
            title: "Test flight".into(),
            confirmation_code: Some("SECRET42".into()),
            status: "confirmed".into(),
            starts_at: None,
            ends_at: None,
            location: None,
            details: Some(json!({ "confirmation_code": "SECRET42" })),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        });
        let serialized = value.to_string();
        assert!(!serialized.contains("SECRET42"));
        assert!(serialized.contains("•••• ET42"));
        assert_eq!(mask_confirmation(Some("A")), Some("••••".into()));
        assert_eq!(mask_confirmation(Some("A1B2")), Some("••••".into()));
        assert_eq!(mask_confirmation(Some("XA1B2")), Some("•••• A1B2".into()));
        assert_eq!(
            mask_confirmation(Some("SECRET42")),
            Some("•••• ET42".into())
        );
    }

    #[test]
    fn receipt_overrides_distinguish_omitted_values_from_explicit_nulls() {
        let parsed = ParsedTravelDocument {
            kind: "flight".into(),
            title: "Flight to Paris".into(),
            provider: Some("Air Test".into()),
            confirmation_code: Some("SECRET42".into()),
            status: "confirmed".into(),
            start_at: Some("2026-09-10T08:00:00Z".into()),
            end_at: Some("2026-09-10T16:00:00Z".into()),
            timezone: Some("Europe/Paris".into()),
            origin: Some("JFK".into()),
            destination: Some("CDG".into()),
            address: None,
            service_number: Some("AT100".into()),
            amount_minor: None,
            currency: None,
            confidence: 0.7,
            decision: "quarantined".into(),
            reasons: vec!["review".into()],
            parser_version: "test".into(),
        };

        let mut preserved = parsed.clone();
        let omitted = serde_json::from_value::<ReceiptApprovalRequest>(json!({
            "title": "Updated title"
        }))
        .unwrap();
        apply_receipt_overrides(&mut preserved, omitted);
        assert_eq!(preserved.provider.as_deref(), Some("Air Test"));
        assert_eq!(preserved.confirmation_code.as_deref(), Some("SECRET42"));
        assert_eq!(preserved.end_at.as_deref(), Some("2026-09-10T16:00:00Z"));
        assert_eq!(preserved.origin.as_deref(), Some("JFK"));
        assert_eq!(preserved.destination.as_deref(), Some("CDG"));
        assert_eq!(
            mask_confirmation(preserved.confirmation_code.as_deref()),
            Some("•••• ET42".into())
        );

        let mut cleared = parsed;
        let explicit_nulls = serde_json::from_value::<ReceiptApprovalRequest>(json!({
            "provider": null,
            "confirmation_code": null,
            "end_at": null,
            "origin": null,
            "destination": null
        }))
        .unwrap();
        apply_receipt_overrides(&mut cleared, explicit_nulls);
        assert_eq!(cleared.provider, None);
        assert_eq!(cleared.confirmation_code, None);
        assert_eq!(cleared.end_at, None);
        assert_eq!(cleared.origin, None);
        assert_eq!(cleared.destination, None);
    }

    #[test]
    fn receipt_review_exposes_editable_itinerary_but_masks_confirmation() {
        let parsed = ParsedTravelDocument {
            kind: "flight".into(),
            title: "Flight to Paris".into(),
            provider: Some("Air Test".into()),
            confirmation_code: Some("SECRET42".into()),
            status: "changed".into(),
            start_at: Some("2026-09-10T08:00:00Z".into()),
            end_at: Some("2026-09-10T16:00:00Z".into()),
            timezone: Some("Europe/Paris".into()),
            origin: Some("JFK".into()),
            destination: Some("CDG".into()),
            address: None,
            service_number: Some("AT100".into()),
            amount_minor: None,
            currency: None,
            confidence: 0.7,
            decision: "quarantined".into(),
            reasons: vec!["review".into()],
            parser_version: "test".into(),
        };
        let value = receipt_view(TravelReceipt {
            id: "r1".into(),
            account_id: "gmail".into(),
            folder: "INBOX".into(),
            uidvalidity: 1,
            uid: 2,
            message_id: Some("receipt@example.test".into()),
            content_hash: "hash".into(),
            from_addr: Some("tickets@example.test".into()),
            subject: Some("Flight changed".into()),
            received_at: Some("2026-08-31T12:00:00Z".into()),
            authenticated_sender_domain: Some("example.test".into()),
            body_text: Some("Flight changed. Confirmation: secret42".into()),
            extracted: Some(serde_json::to_value(parsed).unwrap()),
            trip_id: None,
            status: "quarantined".into(),
            quarantine_reason: Some("review".into()),
            quarantined_at: Some("2026-08-31T12:00:00Z".into()),
            created_at: "2026-08-31T12:00:00Z".into(),
            updated_at: "2026-08-31T12:00:00Z".into(),
        });
        assert_eq!(value["title"], "Flight to Paris");
        assert_eq!(value["parsed_status"], "changed");
        assert_eq!(value["start_at"], "2026-09-10T08:00:00Z");
        assert_eq!(value["origin"], "JFK");
        assert_eq!(value["destination"], "CDG");
        assert_eq!(value["excerpt"], "Flight changed. Confirmation: ••••");
        let serialized = value.to_string();
        assert!(!serialized.contains("SECRET42"));
        assert!(serialized.contains("•••• ET42"));
    }

    #[test]
    fn trip_grouping_uses_location_without_splitting_sequential_legs() {
        let trip = envelope_email_store::Trip {
            id: "t".into(),
            title: "Paris".into(),
            destination: Some("Paris".into()),
            starts_at: Some("2026-09-10T00:00:00Z".into()),
            ends_at: Some("2026-09-15T00:00:00Z".into()),
            timezone: "Europe/Paris".into(),
            status: "upcoming".into(),
            notes: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        };
        let mut parsed = ParsedTravelDocument {
            kind: "unknown".into(),
            title: String::new(),
            provider: None,
            confirmation_code: None,
            status: "unknown".into(),
            start_at: None,
            end_at: None,
            timezone: None,
            origin: None,
            destination: None,
            address: None,
            service_number: None,
            amount_minor: None,
            currency: None,
            confidence: 0.0,
            decision: "quarantined".into(),
            reasons: Vec::new(),
            parser_version: "test".into(),
        };
        parsed.destination = Some("Paris".into());
        parsed.start_at = Some("2026-09-14T00:00:00Z".into());
        assert!(trip_matches(&trip, &parsed));

        // An unrelated family member's overlapping hotel must not be folded
        // into this trip just because its dates overlap.
        parsed.destination = Some("Tokyo".into());
        parsed.start_at = Some("2026-09-12T00:00:00Z".into());
        parsed.end_at = Some("2026-09-14T00:00:00Z".into());
        assert!(!trip_matches(&trip, &parsed));

        // A new city beginning at the end of the existing itinerary is a
        // legitimate multi-leg continuation even without a shared city name.
        parsed.destination = Some("London".into());
        parsed.start_at = Some("2026-09-14T18:00:00Z".into());
        parsed.end_at = Some("2026-09-17T10:00:00Z".into());
        assert!(trip_matches(&trip, &parsed));

        // Airport codes are weak evidence, so they must not split an otherwise
        // unambiguous trip whose human destination is a city name.
        parsed.destination = Some("CDG".into());
        parsed.start_at = Some("2026-09-12T00:00:00Z".into());
        parsed.end_at = Some("2026-09-12T06:00:00Z".into());
        assert!(trip_matches(&trip, &parsed));

        parsed.start_at = Some("2027-09-12T00:00:00Z".into());
        assert!(!trip_matches(&trip, &parsed));
    }

    #[test]
    fn overlapping_family_trips_require_a_unique_location_match() {
        let paris = envelope_email_store::Trip {
            id: "paris".into(),
            title: "Paris".into(),
            destination: Some("Paris".into()),
            starts_at: Some("2026-09-10T00:00:00Z".into()),
            ends_at: Some("2026-09-15T00:00:00Z".into()),
            timezone: "Europe/Paris".into(),
            status: "upcoming".into(),
            notes: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        };
        let tokyo = envelope_email_store::Trip {
            id: "tokyo".into(),
            title: "Tokyo".into(),
            destination: Some("Tokyo".into()),
            ..paris.clone()
        };
        let mut parsed = ParsedTravelDocument {
            kind: "hotel".into(),
            title: "Tokyo hotel".into(),
            provider: Some("Example Hotel".into()),
            confirmation_code: None,
            status: "confirmed".into(),
            start_at: Some("2026-09-12T00:00:00Z".into()),
            end_at: Some("2026-09-14T00:00:00Z".into()),
            timezone: None,
            origin: None,
            destination: Some("Tokyo".into()),
            address: None,
            service_number: None,
            amount_minor: None,
            currency: None,
            confidence: 0.99,
            decision: "auto_accepted".into(),
            reasons: Vec::new(),
            parser_version: "test".into(),
        };
        let trips = vec![paris, tokyo];
        assert_eq!(
            select_matching_trip(&trips, &parsed).map(|trip| trip.id.as_str()),
            Some("tokyo")
        );

        parsed.kind = "flight".into();
        parsed.destination = Some("HND".into());
        assert!(select_matching_trip(&trips, &parsed).is_none());
    }

    #[test]
    fn bounded_sync_consumes_oldest_unseen_uids_without_cursor_leaps() {
        assert_eq!(next_scan_uids(&[90, 12, 30, 29, 42], 29, 2), vec![30, 42]);
        assert_eq!(next_scan_uids(&[90, 12, 30, 29, 42], 42, 2), vec![90]);
    }

    #[test]
    fn first_sync_bootstraps_from_recent_mail() {
        assert_eq!(latest_scan_uids(&[90, 12, 30, 29, 42], 3), vec![30, 42, 90]);
    }

    #[test]
    fn historical_backfill_also_checks_the_newest_mail_without_cursor_leaps() {
        let available = [1, 2, 3, 4, 98, 99, 100];
        let backfill = next_scan_uids(&available, 1, 2);
        assert_eq!(backfill, vec![2, 3]);
        let scanned = merge_scan_uids(backfill.clone(), latest_scan_uids(&available, 2));
        assert_eq!(scanned, vec![2, 3, 99, 100]);
        // The caller commits the backfill tail (3), never the recent safety
        // window's tail (100), so UIDs 4..98 remain eligible for later sweeps.
        assert_eq!(backfill.last().copied(), Some(3));
    }

    #[test]
    fn missing_batch_members_are_retried_individually() {
        let fetched = [2, 4].into_iter().collect::<HashSet<_>>();
        assert_eq!(missing_requested_uids(&[1, 2, 3, 4], &fetched), vec![1, 3]);
    }

    #[test]
    fn singleton_deterministic_fetch_failure_is_isolated_and_countable() {
        let fetched = classify_travel_fetch_result::<()>(
            Vec::new(),
            vec![(42, "UID FETCH returned no raw message".into())],
            Some("UID FETCH parse error for message 42"),
        );
        assert!(fetched.items.is_empty());
        assert!(fetched.sweep_error.is_none());
        assert_eq!(
            fetched.isolated_failures,
            vec![(42, "UID FETCH returned no raw message".into())]
        );
    }

    #[test]
    fn common_connection_failure_is_a_sweep_error_not_uid_failures() {
        let fetched = classify_travel_fetch_result::<()>(
            Vec::new(),
            vec![
                (41, "connection reset during UID FETCH 41".into()),
                (42, "connection reset during UID FETCH 42".into()),
            ],
            Some("IMAP connection reset by peer"),
        );
        assert!(fetched.items.is_empty());
        assert!(fetched.isolated_failures.is_empty());
        assert!(fetched.sweep_error.is_some());
    }

    #[test]
    fn gmail_limit_response_is_a_sweep_error_not_a_uid_failure() {
        let fetched = classify_travel_fetch_result::<()>(
            Vec::new(),
            vec![(42, "NO [LIMIT] Too many simultaneous connections".into())],
            Some("IMAP protocol error: NO [LIMIT] Too many simultaneous connections"),
        );
        assert!(fetched.items.is_empty());
        assert!(fetched.isolated_failures.is_empty());
        assert!(fetched.sweep_error.is_some());
    }

    #[test]
    fn gmail_unavailable_response_is_a_sweep_error_not_a_uid_failure() {
        let fetched = classify_travel_fetch_result::<()>(
            Vec::new(),
            vec![(42, "NO [UNAVAILABLE] Temporary System Error".into())],
            Some("IMAP protocol error: NO [UNAVAILABLE] Temporary System Error"),
        );
        assert!(fetched.items.is_empty());
        assert!(fetched.isolated_failures.is_empty());
        assert!(fetched.sweep_error.is_some());
    }

    #[test]
    fn identical_missing_uids_after_a_healthy_batch_remain_isolated() {
        let fetched = classify_travel_fetch_result::<()>(
            Vec::new(),
            vec![
                (41, "UID FETCH returned no raw message".into()),
                (42, "UID FETCH returned no raw message".into()),
            ],
            None,
        );
        assert!(fetched.sweep_error.is_none());
        assert_eq!(fetched.isolated_failures.len(), 2);
        assert_eq!(fetched.isolated_failures[0].0, 41);
        assert_eq!(fetched.isolated_failures[1].0, 42);
    }

    #[test]
    fn identical_missing_body_failures_remain_isolated_after_batch_parse_error() {
        let fetched = classify_travel_fetch_result::<()>(
            Vec::new(),
            vec![
                (41, "UID FETCH returned message without BODY".into()),
                (42, "UID FETCH returned message without BODY".into()),
            ],
            Some("UID FETCH returned message without BODY"),
        );
        assert!(fetched.sweep_error.is_none());
        assert_eq!(fetched.isolated_failures.len(), 2);
        assert_eq!(fetched.isolated_failures[0].0, 41);
        assert_eq!(fetched.isolated_failures[1].0, 42);
    }

    #[tokio::test]
    async fn whole_batch_outage_does_not_consume_per_uid_poison_budget() {
        let state = AppState::new(
            envelope_email_store::Database::open_memory().unwrap(),
            envelope_email_store::CredentialBackend::File,
        );
        for _ in 0..MAX_MESSAGE_INGEST_ATTEMPTS {
            let fetched = classify_travel_fetch_result::<()>(
                Vec::new(),
                vec![
                    (41, "connection reset during UID FETCH 41".into()),
                    (42, "connection reset during UID FETCH 42".into()),
                ],
                Some("IMAP connection reset by peer"),
            );
            assert!(fetched.items.is_empty());
            assert!(fetched.isolated_failures.is_empty());
            assert!(fetched.sweep_error.is_some());
            // This is the same loop the sync path uses. A sweep-level outage
            // yields no per-UID work, so repeated provider downtime cannot
            // manufacture quarantined receipts.
            for (uid, error) in fetched.isolated_failures {
                let mut result = SyncResult::default();
                let mut folder_blocked = false;
                record_travel_message_failure(
                    &state,
                    &mut result,
                    &mut folder_blocked,
                    TravelMessageFailure {
                        source: TravelMessageSourceKey {
                            account_id: "gmail".into(),
                            folder: "INBOX".into(),
                            uidvalidity: 7,
                            uid: i64::from(uid),
                        },
                        stage: "raw_fetch",
                        error,
                        summary: None,
                    },
                    "2026-08-31T12:00:00Z",
                )
                .await;
            }
        }
        let db = state.db.lock().await;
        for uid in [41, 42] {
            assert!(
                db.get_travel_ingest_failure(&TravelMessageSourceKey {
                    account_id: "gmail".into(),
                    folder: "INBOX".into(),
                    uidvalidity: 7,
                    uid,
                })
                .unwrap()
                .is_none()
            );
        }
        assert!(db.list_travel_receipts(None, true, 10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn poison_message_retries_then_quarantines_without_blocking_cursor() {
        let state = AppState::new(
            envelope_email_store::Database::open_memory().unwrap(),
            envelope_email_store::CredentialBackend::File,
        );
        let source = TravelMessageSourceKey {
            account_id: "gmail".into(),
            folder: "INBOX".into(),
            uidvalidity: 77,
            uid: 42,
        };
        let summary = MessageSummary {
            uid: 42,
            message_id: Some("poison@example.test".into()),
            from_addr: "Travel Co <tickets@example.test>".into(),
            to_addr: "traveler@example.test".into(),
            subject: "Your travel confirmation".into(),
            date: Some("2026-08-30T10:00:00Z".into()),
            flags: Vec::new(),
            size: 100,
            provider_spam: None,
        };
        let mut result = SyncResult {
            status: "complete".into(),
            last_sync_at: "2026-08-31T12:00:00Z".into(),
            ..SyncResult::default()
        };

        for attempt in 1..MAX_MESSAGE_INGEST_ATTEMPTS {
            let mut folder_blocked = false;
            let fetched = classify_travel_fetch_result(
                Vec::<()>::new(),
                vec![(42, "server returned no RFC822 body".into())],
                None,
            );
            assert!(fetched.sweep_error.is_none());
            for (_, error) in fetched.isolated_failures {
                record_travel_message_failure(
                    &state,
                    &mut result,
                    &mut folder_blocked,
                    TravelMessageFailure {
                        source: source.clone(),
                        stage: "raw_fetch",
                        error,
                        summary: Some(summary.clone()),
                    },
                    "2026-08-31T12:00:00Z",
                )
                .await;
            }
            assert!(folder_blocked, "attempt {attempt} must preserve the cursor");
        }

        let mut folder_blocked = false;
        let fetched = classify_travel_fetch_result(
            Vec::<()>::new(),
            vec![(42, "server returned no RFC822 body".into())],
            None,
        );
        assert!(fetched.sweep_error.is_none());
        for (_, error) in fetched.isolated_failures {
            record_travel_message_failure(
                &state,
                &mut result,
                &mut folder_blocked,
                TravelMessageFailure {
                    source: source.clone(),
                    stage: "raw_fetch",
                    error,
                    summary: Some(summary.clone()),
                },
                "2026-08-31T12:02:00Z",
            )
            .await;
        }
        assert!(
            !folder_blocked,
            "durable quarantine must let the backfill cursor progress"
        );
        assert_eq!(result.receipts_imported, 1);
        assert_eq!(result.needs_review, 1);

        let db = state.db.lock().await;
        let receipts = db.list_travel_receipts(None, true, 10).unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].status, "quarantined");
        assert_eq!(receipts[0].uid, 42);
        assert!(
            receipts[0]
                .quarantine_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("after 3 attempts"))
        );
        assert_eq!(db.list_travel_alerts(None).unwrap().len(), 1);
        assert_eq!(
            db.list_travel_alerts(None).unwrap()[0].kind,
            "receipt_review"
        );
        assert_eq!(
            db.list_handled_travel_receipt_sources("gmail", "INBOX", 77)
                .unwrap(),
            Vec::<TravelMessageSourceKey>::new()
        );
        assert_eq!(
            db.list_quarantined_travel_ingest_sources("gmail", "INBOX", 77)
                .unwrap(),
            vec![source.clone()]
        );
        let failure = db.get_travel_ingest_failure(&source).unwrap().unwrap();
        assert_eq!(failure.attempt_count, 3);
        assert!(failure.quarantined_at.is_some());
    }

    #[tokio::test]
    async fn newer_cancellation_survives_older_confirmation_and_change_backfill() {
        for stale_status in ["confirmed", "changed"] {
            let state = AppState::new(
                envelope_email_store::Database::open_memory().unwrap(),
                envelope_email_store::CredentialBackend::File,
            );
            let confirmation_code = format!("MONO-{stale_status}");
            let confirmed = ParsedTravelDocument {
                kind: "flight".into(),
                title: "Flight JFK to CDG".into(),
                provider: Some("Air Test".into()),
                confirmation_code: Some(confirmation_code.clone()),
                status: "confirmed".into(),
                start_at: Some("2026-09-10T08:00:00Z".into()),
                end_at: Some("2026-09-10T16:00:00Z".into()),
                timezone: Some("UTC".into()),
                origin: Some("JFK".into()),
                destination: Some("CDG".into()),
                address: None,
                service_number: Some("AT100".into()),
                amount_minor: None,
                currency: None,
                confidence: 0.99,
                decision: "auto_accepted".into(),
                reasons: vec!["structured_fields_extracted".into()],
                parser_version: "test".into(),
            };
            persist_parsed_document(
                &state,
                TravelSource {
                    account_id: "gmail".into(),
                    folder: "INBOX".into(),
                    uidvalidity: 7,
                    uid: 10,
                    message_id: Some(format!("initial-{stale_status}@example.test")),
                    from_addr: "Air Test <tickets@example.test>".into(),
                    subject: "Flight confirmed".into(),
                    received_at: Some("2026-08-30T08:00:00Z".into()),
                    authenticated_sender_domain: Some("example.test".into()),
                    body_text: format!("Record locator: {confirmation_code}"),
                },
                confirmed.clone(),
                false,
            )
            .await
            .unwrap();

            let cancellation = ParsedTravelDocument {
                kind: "unknown".into(),
                title: "Reservation cancelled".into(),
                provider: None,
                confirmation_code: Some(confirmation_code.clone()),
                status: "cancelled".into(),
                start_at: None,
                end_at: None,
                timezone: None,
                origin: None,
                destination: None,
                address: None,
                service_number: None,
                amount_minor: None,
                currency: None,
                confidence: 0.7,
                decision: "quarantined".into(),
                reasons: vec!["change_notice_needs_identity_match".into()],
                parser_version: "test".into(),
            };
            let cancellation_result = persist_parsed_document(
                &state,
                TravelSource {
                    account_id: "gmail".into(),
                    folder: "INBOX".into(),
                    uidvalidity: 7,
                    uid: 30,
                    message_id: Some(format!("cancellation-{stale_status}@example.test")),
                    from_addr: "Air Test <tickets@example.test>".into(),
                    subject: "Reservation cancelled".into(),
                    received_at: Some("2026-09-02T08:00:00Z".into()),
                    authenticated_sender_domain: Some("example.test".into()),
                    body_text: format!("Reservation cancelled. PNR: {confirmation_code}"),
                },
                cancellation,
                false,
            )
            .await
            .unwrap();
            let cancellation_receipt_id = cancellation_result["receipt"]["id"]
                .as_str()
                .unwrap()
                .to_string();

            // This source is older by authoritative UID even though its Date
            // header equivalent is later. It models the historical batch
            // arriving after the newest-mail safety sweep already cancelled
            // the booking.
            let mut stale = confirmed;
            stale.status = stale_status.into();
            stale.start_at = Some("2026-09-08T08:00:00Z".into());
            stale.end_at = Some("2026-09-18T16:00:00Z".into());
            let stale_result = persist_parsed_document(
                &state,
                TravelSource {
                    account_id: "gmail".into(),
                    folder: "INBOX".into(),
                    uidvalidity: 7,
                    uid: 20,
                    message_id: Some(format!("stale-{stale_status}@example.test")),
                    from_addr: "Air Test <tickets@example.test>".into(),
                    subject: format!("Stale {stale_status} notice"),
                    received_at: Some("2026-09-03T08:00:00Z".into()),
                    authenticated_sender_domain: Some("example.test".into()),
                    body_text: format!("Record locator: {confirmation_code}"),
                },
                stale,
                false,
            )
            .await
            .unwrap();
            assert_eq!(stale_result["amendment_ignored"], true);
            assert_eq!(stale_result["receipt"]["status"], "processed");

            let db = state.db.lock().await;
            let trip = db.list_trips().unwrap().remove(0);
            assert_eq!(trip.starts_at.as_deref(), Some("2026-09-10T08:00:00Z"));
            assert_eq!(trip.ends_at.as_deref(), Some("2026-09-10T16:00:00Z"));
            let booking = db.list_travel_bookings(&trip.id).unwrap().remove(0);
            assert_eq!(booking.status, "cancelled");
            assert_eq!(
                booking.receipt_id.as_deref(),
                Some(cancellation_receipt_id.as_str())
            );
            assert_eq!(booking.starts_at.as_deref(), Some("2026-09-10T08:00:00Z"));
            assert_eq!(booking.ends_at.as_deref(), Some("2026-09-10T16:00:00Z"));
            let segment = db.list_travel_segments(&trip.id).unwrap().remove(0);
            assert_eq!(segment.status, "cancelled");
            let alerts = db.list_travel_alerts(Some(&trip.id)).unwrap();
            assert!(
                !alerts
                    .iter()
                    .any(|alert| matches!(alert.kind.as_str(), "check_in" | "upcoming"))
            );
            assert!(!alerts.iter().any(|alert| alert.kind == "booking_changed"));
        }
    }

    #[tokio::test]
    async fn quarantined_newest_cancellation_replays_after_confirmation_backfill() {
        let state = AppState::new(
            envelope_email_store::Database::open_memory().unwrap(),
            envelope_email_store::CredentialBackend::File,
        );
        let cancellation = parse_travel_document(
            "Air Test <tickets@example.test>",
            "Your reservation was cancelled",
            Some("2026-09-02T08:00:00Z"),
            "Your reservation was cancelled. PNR: BOOT42",
            None,
            None,
        );
        assert_eq!(cancellation.kind, "unknown");
        assert_eq!(cancellation.status, "cancelled");
        assert_eq!(cancellation.decision, "quarantined");
        let cancellation_source = TravelSource {
            account_id: "gmail".into(),
            folder: "INBOX".into(),
            uidvalidity: 7,
            uid: 30,
            message_id: Some("bootstrap-cancellation@example.test".into()),
            from_addr: "Air Test <tickets@example.test>".into(),
            subject: "Your reservation was cancelled".into(),
            received_at: Some("2026-09-02T08:00:00Z".into()),
            authenticated_sender_domain: Some("example.test".into()),
            body_text: "Your reservation was cancelled. PNR: BOOT42".into(),
        };

        // The first-run newest-mail sweep sees the cancellation before its
        // historical confirmation. It must remain replayable, not become a
        // permanent newest-window skip merely because it needs review today.
        let first = persist_parsed_document(
            &state,
            cancellation_source.clone(),
            cancellation.clone(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(first["receipt"]["status"], "quarantined");
        assert_eq!(first["needs_review"], true);
        let cancellation_receipt_id = first["receipt"]["id"].as_str().unwrap().to_string();
        {
            let db = state.db.lock().await;
            assert!(
                db.list_handled_travel_receipt_sources("gmail", "INBOX", 7)
                    .unwrap()
                    .is_empty()
            );
            assert!(
                db.list_quarantined_travel_ingest_sources("gmail", "INBOX", 7)
                    .unwrap()
                    .is_empty(),
                "ordinary review quarantine must not masquerade as a poison-mail skip"
            );
        }

        let confirmation = ParsedTravelDocument {
            kind: "flight".into(),
            title: "Flight JFK to CDG".into(),
            provider: Some("Air Test".into()),
            confirmation_code: Some("BOOT42".into()),
            status: "confirmed".into(),
            start_at: Some("2026-09-10T08:00:00Z".into()),
            end_at: Some("2026-09-10T16:00:00Z".into()),
            timezone: Some("UTC".into()),
            origin: Some("JFK".into()),
            destination: Some("CDG".into()),
            address: None,
            service_number: Some("AT100".into()),
            amount_minor: None,
            currency: None,
            confidence: 0.99,
            decision: "auto_accepted".into(),
            reasons: vec!["structured_fields_extracted".into()],
            parser_version: "test".into(),
        };
        persist_parsed_document(
            &state,
            TravelSource {
                account_id: "gmail".into(),
                folder: "INBOX".into(),
                uidvalidity: 7,
                uid: 10,
                message_id: Some("historical-confirmation@example.test".into()),
                from_addr: "Air Test <tickets@example.test>".into(),
                subject: "Flight confirmed".into(),
                received_at: Some("2026-08-30T08:00:00Z".into()),
                authenticated_sender_domain: Some("example.test".into()),
                body_text: "Record locator: BOOT42".into(),
            },
            confirmation,
            false,
        )
        .await
        .unwrap();
        {
            let db = state.db.lock().await;
            let trip = db.list_trips().unwrap().remove(0);
            let booking = db.list_travel_bookings(&trip.id).unwrap().remove(0);
            assert_eq!(booking.status, "confirmed");
            assert!(
                db.list_travel_alerts(Some(&trip.id))
                    .unwrap()
                    .iter()
                    .any(|alert| matches!(alert.kind.as_str(), "check_in" | "upcoming"))
            );
        }

        // The next newest-mail sweep reconsiders the same cancellation. Its
        // newer authenticated UID can now resolve against BOOT42 and must
        // atomically retire both the reminder and its prior review alert.
        let replay = persist_parsed_document(&state, cancellation_source, cancellation, false)
            .await
            .unwrap();
        assert_eq!(replay["inserted"], false);
        assert_eq!(replay["receipt"]["status"], "processed");
        assert_eq!(replay["needs_review"], false);

        let db = state.db.lock().await;
        let trip = db.list_trips().unwrap().remove(0);
        let booking = db.list_travel_bookings(&trip.id).unwrap().remove(0);
        assert_eq!(booking.status, "cancelled");
        assert_eq!(
            booking.receipt_id.as_deref(),
            Some(cancellation_receipt_id.as_str())
        );
        assert_eq!(
            db.list_travel_segments(&trip.id).unwrap().remove(0).status,
            "cancelled"
        );
        let alerts = db.list_travel_alerts(None).unwrap();
        assert!(alerts.iter().any(|alert| alert.kind == "booking_cancelled"));
        assert!(
            !alerts
                .iter()
                .any(|alert| matches!(alert.kind.as_str(), "check_in" | "upcoming"))
        );
        let review = alerts
            .iter()
            .find(|alert| alert.dedupe_key == format!("receipt:{cancellation_receipt_id}:review"))
            .expect("bootstrap cancellation review alert");
        assert!(review.acknowledged_at.is_some());
    }

    #[test]
    fn scan_folder_is_trimmed_and_control_characters_are_rejected() {
        assert_eq!(
            normalize_scan_folder("  [Gmail]/All Mail  ").unwrap(),
            "[Gmail]/All Mail"
        );
        assert!(normalize_scan_folder("INBOX\r\nUID SEARCH ALL").is_err());
        assert!(normalize_scan_folder("   ").is_err());
    }

    #[tokio::test]
    async fn matching_booking_expands_existing_trip_bounds() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let trip = db
            .create_trip(&NewTrip {
                id: "trip-bounds".into(),
                title: "Paris".into(),
                destination: Some("Paris".into()),
                starts_at: Some("2026-09-10T08:00:00Z".into()),
                ends_at: Some("2026-09-10T16:00:00Z".into()),
                timezone: "Europe/Paris".into(),
                status: "upcoming".into(),
                notes: Some("keep this".into()),
                now: "2026-01-01T00:00:00Z".into(),
            })
            .unwrap();
        let state = AppState::new(db, envelope_email_store::CredentialBackend::File);
        let parsed = ParsedTravelDocument {
            kind: "hotel".into(),
            title: "Paris hotel".into(),
            provider: Some("Example Hotel".into()),
            confirmation_code: Some("PARIS42".into()),
            status: "confirmed".into(),
            start_at: Some("2026-09-09T12:00:00Z".into()),
            end_at: Some("2026-09-15T10:00:00Z".into()),
            timezone: Some("Europe/Paris".into()),
            origin: None,
            destination: Some("Paris".into()),
            address: None,
            service_number: None,
            amount_minor: None,
            currency: None,
            confidence: 0.99,
            decision: "auto_accepted".into(),
            reasons: Vec::new(),
            parser_version: "test".into(),
        };

        let updated = find_or_create_trip(&state, &parsed, "Europe/Paris", "2026-08-31T12:00:00Z")
            .await
            .unwrap();
        assert_eq!(updated.id, trip.id);
        assert_eq!(updated.starts_at.as_deref(), Some("2026-09-09T12:00:00Z"));
        assert_eq!(updated.ends_at.as_deref(), Some("2026-09-15T10:00:00Z"));
        assert_eq!(updated.notes.as_deref(), Some("keep this"));
    }

    #[tokio::test]
    async fn manual_booking_expands_trip_bounds_and_schedules_a_reminder() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        db.create_trip(&NewTrip {
            id: "manual-trip".into(),
            title: "Family trip".into(),
            destination: None,
            starts_at: None,
            ends_at: None,
            timezone: "Europe/London".into(),
            status: "planning".into(),
            notes: None,
            now: "2026-01-01T00:00:00Z".into(),
        })
        .unwrap();
        db.upsert_travel_settings(&TravelSettings {
            account_id: None,
            scan_folder: "INBOX".into(),
            home_timezone: "Europe/London".into(),
            calendar_name: "Family travel".into(),
            auto_ingest: true,
            default_alert_minutes: 120,
            updated_at: "2026-08-31T12:00:00Z".into(),
        })
        .unwrap();
        let state = AppState::new(db, envelope_email_store::CredentialBackend::File);

        let response = create_booking(
            State(state.clone()),
            Json(BookingRequest {
                trip_id: "manual-trip".into(),
                kind: "hotel".into(),
                provider: Some("Example Hotel".into()),
                title: "London hotel".into(),
                confirmation_code: Some("MANUAL42".into()),
                status: Some("confirmed".into()),
                starts_at: Some("2026-09-10T12:00:00Z".into()),
                ends_at: Some("2026-09-14T10:00:00Z".into()),
                location: Some("London".into()),
                origin: None,
                destination: Some("London".into()),
                service_number: None,
                details: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);

        let db = state.db.lock().await;
        let trip = db.get_trip("manual-trip").unwrap().unwrap();
        assert_eq!(trip.destination.as_deref(), Some("London"));
        assert_eq!(trip.starts_at.as_deref(), Some("2026-09-10T12:00:00Z"));
        assert_eq!(trip.ends_at.as_deref(), Some("2026-09-14T10:00:00Z"));
        let booking = db.list_travel_bookings("manual-trip").unwrap().remove(0);
        let reminders = db
            .list_travel_alerts(Some("manual-trip"))
            .unwrap()
            .into_iter()
            .filter(|alert| matches!(alert.kind.as_str(), "check_in" | "upcoming"))
            .collect::<Vec<_>>();
        assert_eq!(reminders.len(), 1);
        assert_eq!(
            reminders[0].booking_id.as_deref(),
            Some(booking.id.as_str())
        );
        assert_eq!(
            reminders[0].scheduled_at.as_deref(),
            Some("2026-09-10T10:00:00+00:00")
        );
    }

    #[tokio::test]
    async fn pnr_date_change_expands_bounds_and_replaces_stale_reminder() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let state = AppState::new(db, envelope_email_store::CredentialBackend::File);
        let initial = ParsedTravelDocument {
            kind: "flight".into(),
            title: "Flight JFK to CDG".into(),
            provider: Some("Air Test".into()),
            confirmation_code: Some("MOVE42".into()),
            status: "confirmed".into(),
            start_at: Some("2026-09-10T08:00:00Z".into()),
            end_at: Some("2026-09-10T16:00:00Z".into()),
            timezone: Some("UTC".into()),
            origin: Some("JFK".into()),
            destination: Some("CDG".into()),
            address: None,
            service_number: Some("AT100".into()),
            amount_minor: None,
            currency: None,
            confidence: 0.99,
            decision: "auto_accepted".into(),
            reasons: vec!["structured_fields_extracted".into()],
            parser_version: "test".into(),
        };
        persist_parsed_document(
            &state,
            TravelSource {
                account_id: "gmail".into(),
                folder: "INBOX".into(),
                uidvalidity: 7,
                uid: 10,
                message_id: Some("move-confirmation@example.test".into()),
                from_addr: "Air Test <tickets@example.test>".into(),
                subject: "Flight confirmed".into(),
                received_at: Some("2026-08-31T08:00:00Z".into()),
                authenticated_sender_domain: Some("example.test".into()),
                body_text: "Record locator: MOVE42".into(),
            },
            initial.clone(),
            false,
        )
        .await
        .unwrap();

        let old_reminder_id = {
            let db = state.db.lock().await;
            db.list_travel_alerts(None)
                .unwrap()
                .into_iter()
                .find(|alert| alert.kind == "upcoming")
                .unwrap()
                .id
        };
        let amended = ParsedTravelDocument {
            title: "Flight JFK to CDG changed".into(),
            status: "changed".into(),
            start_at: Some("2026-09-08T08:00:00Z".into()),
            end_at: Some("2026-09-16T16:00:00Z".into()),
            ..initial
        };
        persist_parsed_document(
            &state,
            TravelSource {
                account_id: "gmail".into(),
                folder: "INBOX".into(),
                uidvalidity: 7,
                uid: 11,
                message_id: Some("move-amendment@example.test".into()),
                from_addr: "Air Test <tickets@example.test>".into(),
                subject: "Flight changed".into(),
                received_at: Some("2026-09-01T08:00:00Z".into()),
                authenticated_sender_domain: Some("example.test".into()),
                body_text: "Updated record locator: MOVE42".into(),
            },
            amended,
            false,
        )
        .await
        .unwrap();

        let db = state.db.lock().await;
        let trip = db.list_trips().unwrap().remove(0);
        assert_eq!(trip.starts_at.as_deref(), Some("2026-09-08T08:00:00Z"));
        assert_eq!(trip.ends_at.as_deref(), Some("2026-09-16T16:00:00Z"));
        let alerts = db.list_travel_alerts(Some(&trip.id)).unwrap();
        let reminders = alerts
            .iter()
            .filter(|alert| matches!(alert.kind.as_str(), "check_in" | "upcoming"))
            .collect::<Vec<_>>();
        assert_eq!(reminders.len(), 1);
        assert_ne!(reminders[0].id, old_reminder_id);
        assert_eq!(
            reminders[0].scheduled_at.as_deref(),
            Some("2026-09-08T06:00:00+00:00")
        );
        assert!(alerts.iter().any(|alert| alert.kind == "booking_changed"));
    }

    #[tokio::test]
    async fn every_sender_domain_requires_the_same_authenticated_mailbox() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let state = AppState::new(db, envelope_email_store::CredentialBackend::File);
        let initial = ParsedTravelDocument {
            kind: "flight".into(),
            title: "Flight JFK to CDG".into(),
            provider: Some("Air Test".into()),
            confirmation_code: Some("PUBLIC42".into()),
            status: "confirmed".into(),
            start_at: Some("2026-09-10T08:00:00Z".into()),
            end_at: Some("2026-09-10T16:00:00Z".into()),
            timezone: Some("UTC".into()),
            origin: Some("JFK".into()),
            destination: Some("CDG".into()),
            address: None,
            service_number: Some("AT100".into()),
            amount_minor: None,
            currency: None,
            confidence: 0.99,
            decision: "auto_accepted".into(),
            reasons: vec!["structured_fields_extracted".into()],
            parser_version: "test".into(),
        };
        persist_parsed_document(
            &state,
            TravelSource {
                account_id: "gmail".into(),
                folder: "INBOX".into(),
                uidvalidity: 7,
                uid: 20,
                message_id: Some("public-confirmation@example.test".into()),
                from_addr: "Air Test <Notices@Air.Example>".into(),
                subject: "Flight confirmed".into(),
                received_at: Some("2026-08-31T08:00:00Z".into()),
                authenticated_sender_domain: Some("air.example".into()),
                body_text: "Record locator: PUBLIC42".into(),
            },
            initial.clone(),
            false,
        )
        .await
        .unwrap();

        let forged = ParsedTravelDocument {
            title: "Flight cancelled".into(),
            status: "cancelled".into(),
            ..initial
        };
        let outcome = persist_parsed_document(
            &state,
            TravelSource {
                account_id: "gmail".into(),
                folder: "INBOX".into(),
                uidvalidity: 7,
                uid: 21,
                message_id: Some("public-forgery@example.test".into()),
                from_addr: "Air Test <attacker@air.example>".into(),
                subject: "Flight cancelled".into(),
                received_at: Some("2026-09-01T08:00:00Z".into()),
                authenticated_sender_domain: Some("air.example".into()),
                body_text: "Record locator: PUBLIC42".into(),
            },
            forged,
            false,
        )
        .await
        .unwrap();

        assert_eq!(outcome["needs_review"], true);
        assert_eq!(outcome["receipt"]["status"], "quarantined");
        let db = state.db.lock().await;
        let trip = db.list_trips().unwrap().remove(0);
        let booking = db.list_travel_bookings(&trip.id).unwrap().remove(0);
        assert_eq!(booking.status, "confirmed");
        let quarantined = db.list_quarantined_travel_receipts(10).unwrap();
        assert_eq!(quarantined.len(), 1);
        assert_eq!(
            quarantined[0].quarantine_reason.as_deref(),
            Some("automatic_amendment_sender_mailbox_mismatch")
        );
    }

    #[test]
    fn same_trip_reused_pnr_with_incompatible_dates_creates_a_new_booking() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let trip = db
            .create_trip(&NewTrip {
                id: "long-running-trip".into(),
                title: "Long-running family travel".into(),
                destination: Some("Europe".into()),
                starts_at: Some("2020-09-10T15:00:00Z".into()),
                ends_at: Some("2027-10-13T10:00:00Z".into()),
                timezone: "UTC".into(),
                status: "upcoming".into(),
                notes: None,
                now: "2020-01-01T00:00:00Z".into(),
            })
            .unwrap();
        db.create_travel_booking(&NewTravelBooking {
            id: "old-reused-pnr-booking".into(),
            trip_id: trip.id.clone(),
            receipt_id: None,
            kind: "hotel".into(),
            provider: Some("Example Hotel".into()),
            title: "Old Paris hotel".into(),
            confirmation_code: Some("SAME42".into()),
            status: "completed".into(),
            starts_at: Some("2020-09-10T15:00:00Z".into()),
            ends_at: Some("2020-09-13T10:00:00Z".into()),
            location: Some("Paris".into()),
            details: None,
            now: "2020-01-01T00:00:00Z".into(),
        })
        .unwrap();
        db.ingest_travel_receipt(&NewTravelReceipt {
            id: "new-reused-pnr-receipt".into(),
            account_id: "gmail".into(),
            folder: "INBOX".into(),
            uidvalidity: 7,
            uid: 50,
            message_id: Some("same-trip-reused@example.test".into()),
            from_addr: Some("Example Hotel <notices@hotel.example>".into()),
            subject: Some("New hotel confirmed".into()),
            received_at: Some("2026-10-01T08:00:00Z".into()),
            authenticated_sender_domain: Some("hotel.example".into()),
            body_text: Some("Confirmation: SAME42".into()),
            extracted: None,
            trip_id: None,
            quarantine_reason: None,
            ingested_at: "2026-10-01T08:00:00Z".into(),
        })
        .unwrap();
        let parsed = ParsedTravelDocument {
            kind: "hotel".into(),
            title: "New London hotel".into(),
            provider: Some("Example Hotel".into()),
            confirmation_code: Some("SAME42".into()),
            status: "confirmed".into(),
            start_at: Some("2027-10-10T15:00:00Z".into()),
            end_at: Some("2027-10-13T10:00:00Z".into()),
            timezone: Some("Europe/London".into()),
            origin: None,
            destination: Some("London".into()),
            address: Some("London".into()),
            service_number: None,
            amount_minor: None,
            currency: None,
            confidence: 0.99,
            decision: "auto_accepted".into(),
            reasons: vec!["structured_fields_extracted".into()],
            parser_version: "test".into(),
        };

        let (created, booking_created, _) = upsert_booking_on(
            &db,
            &trip.id,
            "new-reused-pnr-receipt",
            &parsed,
            "2026-10-01T08:01:00Z",
        )
        .unwrap();

        assert!(booking_created);
        assert_eq!(created.starts_at.as_deref(), Some("2027-10-10T15:00:00Z"));
        let bookings = db.list_travel_bookings(&trip.id).unwrap();
        assert_eq!(bookings.len(), 2);
        let old = bookings
            .iter()
            .find(|booking| booking.id == "old-reused-pnr-booking")
            .unwrap();
        assert_eq!(old.status, "completed");
        assert_eq!(old.starts_at.as_deref(), Some("2020-09-10T15:00:00Z"));
    }

    #[tokio::test]
    async fn recycled_old_pnr_requires_review_but_owner_can_override() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let state = AppState::new(db, envelope_email_store::CredentialBackend::File);
        let initial = ParsedTravelDocument {
            kind: "hotel".into(),
            title: "Old Paris hotel".into(),
            provider: Some("Example Hotel".into()),
            confirmation_code: Some("RECYCLED42".into()),
            status: "confirmed".into(),
            start_at: Some("2020-09-10T15:00:00Z".into()),
            end_at: Some("2020-09-13T10:00:00Z".into()),
            timezone: Some("Europe/Paris".into()),
            origin: None,
            destination: Some("Paris".into()),
            address: Some("Paris".into()),
            service_number: None,
            amount_minor: None,
            currency: None,
            confidence: 0.99,
            decision: "auto_accepted".into(),
            reasons: vec!["structured_fields_extracted".into()],
            parser_version: "test".into(),
        };
        persist_parsed_document(
            &state,
            TravelSource {
                account_id: "gmail".into(),
                folder: "INBOX".into(),
                uidvalidity: 7,
                uid: 30,
                message_id: Some("old-pnr-confirmation@example.test".into()),
                from_addr: "Example Hotel <notices@hotel.example>".into(),
                subject: "Hotel confirmed".into(),
                received_at: Some("2020-01-01T08:00:00Z".into()),
                authenticated_sender_domain: Some("hotel.example".into()),
                body_text: "Confirmation: RECYCLED42".into(),
            },
            initial,
            false,
        )
        .await
        .unwrap();

        let cancellation = ParsedTravelDocument {
            kind: "unknown".into(),
            title: "Reservation cancelled".into(),
            provider: Some("Example Hotel".into()),
            confirmation_code: Some("RECYCLED42".into()),
            status: "cancelled".into(),
            start_at: None,
            end_at: None,
            timezone: None,
            origin: None,
            destination: None,
            address: None,
            service_number: None,
            amount_minor: None,
            currency: None,
            confidence: 0.7,
            decision: "quarantined".into(),
            reasons: vec!["status_only_notice".into()],
            parser_version: "test".into(),
        };
        let source = TravelSource {
            account_id: "gmail".into(),
            folder: "INBOX".into(),
            uidvalidity: 7,
            uid: 31,
            message_id: Some("recycled-pnr-cancellation@example.test".into()),
            from_addr: "Example Hotel <notices@hotel.example>".into(),
            subject: "Reservation cancelled".into(),
            received_at: Some("2026-09-01T08:00:00Z".into()),
            authenticated_sender_domain: Some("hotel.example".into()),
            body_text: "Confirmation: RECYCLED42".into(),
        };
        let automatic =
            persist_parsed_document(&state, source.clone(), cancellation.clone(), false)
                .await
                .unwrap();
        assert_eq!(automatic["needs_review"], true);
        assert_eq!(automatic["receipt"]["status"], "quarantined");
        {
            let db = state.db.lock().await;
            let trip = db.list_trips().unwrap().remove(0);
            assert_eq!(
                db.list_travel_bookings(&trip.id).unwrap()[0].status,
                "confirmed"
            );
            assert_eq!(
                db.list_quarantined_travel_receipts(10).unwrap()[0]
                    .quarantine_reason
                    .as_deref(),
                Some("automatic_amendment_booking_not_active_or_date_compatible")
            );
        }

        let manual = persist_parsed_document(&state, source, cancellation, true)
            .await
            .unwrap();
        assert_eq!(manual["needs_review"], false);
        assert_eq!(manual["receipt"]["status"], "processed");
        {
            let db = state.db.lock().await;
            let trip = db.list_trips().unwrap().remove(0);
            assert_eq!(
                db.list_travel_bookings(&trip.id).unwrap()[0].status,
                "cancelled"
            );
        }

        // A new, fully dated stay may legitimately reuse the provider's old
        // confirmation code. Incompatible structured dates make it a new
        // identity instead of an amendment to the historical booking.
        let reused = ParsedTravelDocument {
            kind: "hotel".into(),
            title: "New London hotel".into(),
            provider: Some("Example Hotel".into()),
            confirmation_code: Some("RECYCLED42".into()),
            status: "confirmed".into(),
            start_at: Some("2027-10-10T15:00:00Z".into()),
            end_at: Some("2027-10-13T10:00:00Z".into()),
            timezone: Some("Europe/London".into()),
            origin: None,
            destination: Some("London".into()),
            address: Some("London".into()),
            service_number: None,
            amount_minor: None,
            currency: None,
            confidence: 0.99,
            decision: "auto_accepted".into(),
            reasons: vec!["structured_fields_extracted".into()],
            parser_version: "test".into(),
        };
        let reused = persist_parsed_document(
            &state,
            TravelSource {
                account_id: "gmail".into(),
                folder: "INBOX".into(),
                uidvalidity: 7,
                uid: 32,
                message_id: Some("recycled-pnr-new-stay@example.test".into()),
                from_addr: "Example Hotel <notices@hotel.example>".into(),
                subject: "New hotel confirmed".into(),
                received_at: Some("2026-10-01T08:00:00Z".into()),
                authenticated_sender_domain: Some("hotel.example".into()),
                body_text: "Confirmation: RECYCLED42".into(),
            },
            reused,
            false,
        )
        .await
        .unwrap();
        assert_eq!(reused["booking_created"], true);
        assert_eq!(reused["needs_review"], false);
        let db = state.db.lock().await;
        assert_eq!(db.list_trips().unwrap().len(), 2);
        let all_bookings = db
            .list_trips()
            .unwrap()
            .into_iter()
            .flat_map(|trip| db.list_travel_bookings(&trip.id).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(all_bookings.len(), 2);
        assert!(all_bookings.iter().any(|booking| {
            booking.status == "confirmed"
                && booking.starts_at.as_deref() == Some("2027-10-10T15:00:00Z")
        }));
    }

    #[tokio::test]
    async fn undated_pnr_never_stays_automatically_mutable_forever() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let trip = db
            .create_trip(&NewTrip {
                id: "undated-trip".into(),
                title: "Legacy undated booking".into(),
                destination: None,
                starts_at: None,
                ends_at: None,
                timezone: "UTC".into(),
                status: "upcoming".into(),
                notes: None,
                now: "2020-01-01T00:00:00Z".into(),
            })
            .unwrap();
        let receipt = db
            .ingest_travel_receipt(&NewTravelReceipt {
                id: "undated-current-receipt".into(),
                account_id: "gmail".into(),
                folder: "INBOX".into(),
                uidvalidity: 7,
                uid: 40,
                message_id: Some("undated-current@example.test".into()),
                from_addr: Some("Example Hotel <notices@hotel.example>".into()),
                subject: Some("Legacy reservation".into()),
                received_at: Some("2020-01-01T08:00:00Z".into()),
                authenticated_sender_domain: Some("hotel.example".into()),
                body_text: Some("Confirmation: UNDATED42".into()),
                extracted: None,
                trip_id: Some(trip.id.clone()),
                quarantine_reason: None,
                ingested_at: "2020-01-01T08:00:00Z".into(),
            })
            .unwrap()
            .receipt;
        db.mark_travel_receipt_processed(&receipt.id, Some(&trip.id), None, "2020-01-01T08:01:00Z")
            .unwrap();
        db.create_travel_booking(&NewTravelBooking {
            id: "undated-booking".into(),
            trip_id: trip.id.clone(),
            receipt_id: Some(receipt.id),
            kind: "hotel".into(),
            provider: Some("Example Hotel".into()),
            title: "Legacy reservation".into(),
            confirmation_code: Some("UNDATED42".into()),
            status: "confirmed".into(),
            starts_at: None,
            ends_at: None,
            location: None,
            details: None,
            now: "2020-01-01T08:01:00Z".into(),
        })
        .unwrap();
        let state = AppState::new(db, envelope_email_store::CredentialBackend::File);
        let parsed = ParsedTravelDocument {
            kind: "unknown".into(),
            title: "Reservation cancelled".into(),
            provider: Some("Example Hotel".into()),
            confirmation_code: Some("UNDATED42".into()),
            status: "cancelled".into(),
            start_at: None,
            end_at: None,
            timezone: None,
            origin: None,
            destination: None,
            address: None,
            service_number: None,
            amount_minor: None,
            currency: None,
            confidence: 0.7,
            decision: "quarantined".into(),
            reasons: vec!["status_only_notice".into()],
            parser_version: "test".into(),
        };
        let source = TravelSource {
            account_id: "gmail".into(),
            folder: "INBOX".into(),
            uidvalidity: 7,
            uid: 41,
            message_id: Some("undated-cancellation@example.test".into()),
            from_addr: "Example Hotel <notices@hotel.example>".into(),
            subject: "Reservation cancelled".into(),
            received_at: Some("2026-09-01T08:00:00Z".into()),
            authenticated_sender_domain: Some("hotel.example".into()),
            body_text: "Confirmation: UNDATED42".into(),
        };

        let automatic = persist_parsed_document(&state, source.clone(), parsed.clone(), false)
            .await
            .unwrap();
        assert_eq!(automatic["needs_review"], true);
        assert_eq!(automatic["receipt"]["status"], "quarantined");
        {
            let db = state.db.lock().await;
            assert_eq!(
                db.list_travel_bookings(&trip.id).unwrap()[0].status,
                "confirmed"
            );
        }

        let manual = persist_parsed_document(&state, source, parsed, true)
            .await
            .unwrap();
        assert_eq!(manual["needs_review"], false);
        let db = state.db.lock().await;
        assert_eq!(
            db.list_travel_bookings(&trip.id).unwrap()[0].status,
            "cancelled"
        );
    }

    #[tokio::test]
    async fn newly_inserted_receipt_reread_honors_an_intervening_dismissal() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let mut ingest = db
            .ingest_travel_receipt(&NewTravelReceipt {
                id: "new-race-receipt".into(),
                account_id: "gmail".into(),
                folder: "INBOX".into(),
                uidvalidity: 7,
                uid: 99,
                message_id: Some("new-race@example.test".into()),
                from_addr: Some("tickets@example.test".into()),
                subject: Some("New booking".into()),
                received_at: Some("2026-08-31T08:00:00Z".into()),
                authenticated_sender_domain: None,
                body_text: Some("New travel receipt".into()),
                extracted: None,
                trip_id: None,
                quarantine_reason: None,
                ingested_at: "2026-08-31T12:00:00Z".into(),
            })
            .unwrap();
        assert!(ingest.inserted);
        let state = AppState::new(db, envelope_email_store::CredentialBackend::File);

        {
            let _dismiss_operation = state
                .lock_travel_receipt_operation("new-race-receipt")
                .await;
            let db = state.db.lock().await;
            assert!(matches!(
                db.dismiss_travel_receipt("new-race-receipt", "2026-08-31T12:01:00Z")
                    .unwrap(),
                TravelReceiptTerminalTransition::Applied(_)
            ));
        }
        let _persistence_operation = state
            .lock_travel_receipt_operation("new-race-receipt")
            .await;
        ingest.receipt = {
            let db = state.db.lock().await;
            db.get_travel_receipt("new-race-receipt").unwrap().unwrap()
        };

        let automatic = terminal_receipt_persistence_outcome(&ingest, false)
            .unwrap()
            .unwrap();
        assert_eq!(automatic["inserted"], false);
        assert_eq!(automatic["receipt"]["status"], "dismissed");
        assert!(matches!(
            terminal_receipt_persistence_outcome(&ingest, true),
            Err(PersistTravelError::ReceiptConflict(ref receipt))
                if receipt.status == "dismissed"
        ));
        let db = state.db.lock().await;
        assert!(db.list_trips().unwrap().is_empty());
        assert!(db.list_travel_alerts(None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn dismissal_serializes_a_racing_approval_before_itinerary_writes() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let parsed = ParsedTravelDocument {
            kind: "flight".into(),
            title: "Flight JFK to CDG".into(),
            provider: Some("Air Test".into()),
            confirmation_code: Some("RACE42".into()),
            status: "confirmed".into(),
            start_at: Some("2026-09-10T08:00:00Z".into()),
            end_at: Some("2026-09-10T10:00:00Z".into()),
            timezone: Some("UTC".into()),
            origin: Some("JFK".into()),
            destination: Some("CDG".into()),
            address: None,
            service_number: Some("AT100".into()),
            amount_minor: None,
            currency: None,
            confidence: 0.7,
            decision: "quarantined".into(),
            reasons: vec!["review".into()],
            parser_version: "test".into(),
        };
        db.ingest_travel_receipt(&NewTravelReceipt {
            id: "racing-receipt".into(),
            account_id: "gmail".into(),
            folder: "INBOX".into(),
            uidvalidity: 7,
            uid: 42,
            message_id: Some("race@example.test".into()),
            from_addr: Some("Air Test <tickets@example.test>".into()),
            subject: Some("Flight confirmed".into()),
            received_at: Some("2026-08-31T08:00:00Z".into()),
            authenticated_sender_domain: Some("example.test".into()),
            body_text: Some("Record locator: RACE42".into()),
            extracted: Some(serde_json::to_value(&parsed).unwrap()),
            trip_id: None,
            quarantine_reason: Some("initial review".into()),
            ingested_at: "2026-08-31T12:00:00Z".into(),
        })
        .unwrap();
        let state = AppState::new(db, envelope_email_store::CredentialBackend::File);

        // Queue dismissal before approval behind the same keyed lock. The
        // approval duplicate-ingest writes its review reason before waiting,
        // giving the test a deterministic signal that both paths overlap.
        let blocker = state.lock_travel_receipt_operation("racing-receipt").await;
        let (dismiss_queued_tx, dismiss_queued_rx) = tokio::sync::oneshot::channel();
        let dismiss_state = state.clone();
        let dismiss_task = tokio::spawn(async move {
            let _ = dismiss_queued_tx.send(());
            dismiss_receipt(State(dismiss_state), Path("racing-receipt".to_string())).await
        });
        dismiss_queued_rx.await.unwrap();
        tokio::task::yield_now().await;

        let approve_state = state.clone();
        let approve_task = tokio::spawn(async move {
            approve_receipt(
                State(approve_state),
                Path("racing-receipt".to_string()),
                Json(ReceiptApprovalRequest::default()),
            )
            .await
        });
        let approval_reached_lock =
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    let reached = {
                        let db = state.db.lock().await;
                        db.get_travel_receipt("racing-receipt")
                            .unwrap()
                            .is_some_and(|receipt| {
                                receipt.quarantine_reason.as_deref() == Some("approved_by_owner")
                            })
                    };
                    if reached {
                        break true;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap_or(false);
        assert!(
            approval_reached_lock,
            "approval never reached the keyed lock"
        );
        drop(blocker);

        let dismiss_response = dismiss_task.await.unwrap();
        let approve_response = approve_task.await.unwrap();
        assert_eq!(dismiss_response.status(), StatusCode::OK);
        assert_eq!(approve_response.status(), StatusCode::CONFLICT);
        let db = state.db.lock().await;
        assert_eq!(
            db.get_travel_receipt("racing-receipt")
                .unwrap()
                .unwrap()
                .status,
            "dismissed"
        );
        assert!(db.list_trips().unwrap().is_empty());
        assert!(db.list_travel_alerts(None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn duplicate_pending_receipt_resumes_derived_itinerary_work() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let parsed = ParsedTravelDocument {
            kind: "flight".into(),
            title: "Flight JFK to CDG".into(),
            provider: Some("Air Test".into()),
            confirmation_code: Some("RESUME42".into()),
            status: "confirmed".into(),
            start_at: Some("2026-09-10T08:00:00Z".into()),
            end_at: Some("2026-09-10T10:00:00Z".into()),
            timezone: Some("UTC".into()),
            origin: Some("JFK".into()),
            destination: Some("CDG".into()),
            address: None,
            service_number: Some("AT100".into()),
            amount_minor: None,
            currency: None,
            confidence: 0.99,
            decision: "auto_accepted".into(),
            reasons: vec!["structured_fields_extracted".into()],
            parser_version: "test".into(),
        };
        db.ingest_travel_receipt(&NewTravelReceipt {
            id: "pending-receipt".into(),
            account_id: "gmail".into(),
            folder: "INBOX".into(),
            uidvalidity: 7,
            uid: 42,
            message_id: Some("resume@example.test".into()),
            from_addr: Some("Air Test <tickets@example.test>".into()),
            subject: Some("Flight confirmed".into()),
            received_at: Some("2026-08-31T08:00:00Z".into()),
            authenticated_sender_domain: Some("example.test".into()),
            body_text: Some("Record locator: RESUME42".into()),
            extracted: Some(serde_json::to_value(&parsed).unwrap()),
            trip_id: None,
            quarantine_reason: None,
            ingested_at: "2026-08-31T12:00:00Z".into(),
        })
        .unwrap();
        let state = AppState::new(db, envelope_email_store::CredentialBackend::File);
        let result = persist_parsed_document(
            &state,
            TravelSource {
                account_id: "gmail".into(),
                folder: "INBOX".into(),
                uidvalidity: 7,
                uid: 42,
                message_id: Some("resume@example.test".into()),
                from_addr: "Air Test <tickets@example.test>".into(),
                subject: "Flight confirmed".into(),
                received_at: Some("2026-08-31T08:00:00Z".into()),
                authenticated_sender_domain: Some("example.test".into()),
                body_text: "Record locator: RESUME42".into(),
            },
            parsed,
            false,
        )
        .await
        .unwrap();

        assert_eq!(result["inserted"], false);
        assert_eq!(result["booking_created"], true);
        let db = state.db.lock().await;
        assert_eq!(db.list_trips().unwrap().len(), 1);
        assert_eq!(
            db.get_travel_receipt("pending-receipt")
                .unwrap()
                .unwrap()
                .status,
            "processed"
        );
    }

    #[tokio::test]
    async fn terminal_failure_rolls_back_full_itinerary_and_duplicate_replays() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        db.conn()
            .execute_batch(
                "CREATE TRIGGER reject_handler_receipt_terminal_update
                 BEFORE UPDATE OF status ON travel_receipts
                 WHEN NEW.status = 'processed'
                 BEGIN
                   SELECT RAISE(ABORT, 'forced handler terminal transition failure');
                 END;",
            )
            .unwrap();
        let state = AppState::new(db, envelope_email_store::CredentialBackend::File);
        let parsed = ParsedTravelDocument {
            kind: "flight".into(),
            title: "Flight JFK to CDG".into(),
            provider: Some("Air Test".into()),
            confirmation_code: Some("CRASH42".into()),
            status: "confirmed".into(),
            start_at: Some("2026-09-10T08:00:00Z".into()),
            end_at: Some("2026-09-10T10:00:00Z".into()),
            timezone: Some("UTC".into()),
            origin: Some("JFK".into()),
            destination: Some("CDG".into()),
            address: None,
            service_number: Some("AT100".into()),
            amount_minor: None,
            currency: None,
            confidence: 0.99,
            decision: "auto_accepted".into(),
            reasons: vec!["structured_fields_extracted".into()],
            parser_version: "test".into(),
        };
        let source = TravelSource {
            account_id: "gmail".into(),
            folder: "INBOX".into(),
            uidvalidity: 7,
            uid: 84,
            message_id: Some("crash-replay@example.test".into()),
            from_addr: "Air Test <tickets@example.test>".into(),
            subject: "Flight confirmed".into(),
            received_at: Some("2026-08-31T08:00:00Z".into()),
            authenticated_sender_domain: Some("example.test".into()),
            body_text: "Record locator: CRASH42".into(),
        };

        let failed = persist_parsed_document(&state, source.clone(), parsed.clone(), false).await;
        assert!(matches!(failed, Err(PersistTravelError::Message(_))));
        {
            let db = state.db.lock().await;
            let receipt = db.list_travel_receipts(None, true, 10).unwrap().remove(0);
            assert_eq!(receipt.status, "pending");
            assert!(db.list_trips().unwrap().is_empty());
            assert!(db.list_travel_alerts(None).unwrap().is_empty());
            for table in ["travel_bookings", "travel_segments"] {
                let count = db
                    .conn()
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .unwrap();
                assert_eq!(count, 0, "{table} escaped the failed handler transaction");
            }
            db.conn()
                .execute_batch("DROP TRIGGER reject_handler_receipt_terminal_update;")
                .unwrap();
        }

        let replayed = persist_parsed_document(&state, source, parsed, false)
            .await
            .unwrap();
        assert_eq!(replayed["inserted"], false);
        assert_eq!(replayed["booking_created"], true);
        let db = state.db.lock().await;
        let receipt = db.list_travel_receipts(None, true, 10).unwrap().remove(0);
        assert_eq!(receipt.status, "processed");
        assert_eq!(db.list_trips().unwrap().len(), 1);
        assert_eq!(db.list_travel_alerts(None).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn pnr_only_cancellation_updates_booking_without_erasing_itinerary() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let initial = ParsedTravelDocument {
            kind: "flight".into(),
            title: "Flight JFK to CDG".into(),
            provider: Some("Air Test".into()),
            confirmation_code: Some("CANCEL42".into()),
            status: "confirmed".into(),
            start_at: Some("2026-09-10T08:00:00Z".into()),
            end_at: Some("2026-09-10T16:00:00Z".into()),
            timezone: Some("UTC".into()),
            origin: Some("JFK".into()),
            destination: Some("CDG".into()),
            address: None,
            service_number: Some("AT100".into()),
            amount_minor: None,
            currency: None,
            confidence: 0.99,
            decision: "auto_accepted".into(),
            reasons: vec!["structured_fields_extracted".into()],
            parser_version: "test".into(),
        };
        let state = AppState::new(db, envelope_email_store::CredentialBackend::File);
        persist_parsed_document(
            &state,
            TravelSource {
                account_id: "gmail".into(),
                folder: "INBOX".into(),
                uidvalidity: 7,
                uid: 10,
                message_id: Some("confirmation@example.test".into()),
                from_addr: "Air Test <tickets@example.test>".into(),
                subject: "Flight confirmed".into(),
                received_at: Some("2026-08-31T08:00:00Z".into()),
                authenticated_sender_domain: Some("example.test".into()),
                body_text: "Record locator: CANCEL42".into(),
            },
            initial,
            false,
        )
        .await
        .unwrap();

        // Exercise the real generic provider notice: it contains a durable
        // PNR and cancellation status but no flight/hotel/train vocabulary or
        // dates, so the parser intentionally leaves its kind unknown.
        let cancellation = parse_travel_document(
            "Air Test <tickets@example.test>",
            "Your reservation was cancelled",
            Some("2026-09-01T08:00:00Z"),
            "Your reservation was cancelled. PNR: CANCEL42",
            None,
            None,
        );
        assert_eq!(cancellation.kind, "unknown");
        assert_eq!(cancellation.status, "cancelled");
        assert_eq!(cancellation.confirmation_code.as_deref(), Some("CANCEL42"));
        assert!(cancellation.start_at.is_none());
        let outcome = persist_parsed_document(
            &state,
            TravelSource {
                account_id: "gmail".into(),
                folder: "INBOX".into(),
                uidvalidity: 7,
                uid: 11,
                message_id: Some("cancellation@example.test".into()),
                from_addr: "Air Test <tickets@example.test>".into(),
                subject: "Your reservation was cancelled".into(),
                received_at: Some("2026-09-01T08:00:00Z".into()),
                authenticated_sender_domain: Some("example.test".into()),
                body_text: "Your reservation was cancelled. PNR: CANCEL42".into(),
            },
            cancellation.clone(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(outcome["booking_created"], false);
        assert_eq!(outcome["needs_review"], false);

        // A later backfill can discover an older notice after the current
        // booking has already advanced. It is durably processed as a stale
        // no-op and cannot regress the booking or recreate its reminder.
        let stale = ParsedTravelDocument {
            status: "confirmed".into(),
            ..cancellation
        };
        let stale_outcome = persist_parsed_document(
            &state,
            TravelSource {
                account_id: "gmail".into(),
                folder: "INBOX".into(),
                uidvalidity: 7,
                uid: 5,
                message_id: Some("stale-confirmation@example.test".into()),
                from_addr: "Air Test <tickets@example.test>".into(),
                subject: "Your reservation is confirmed".into(),
                received_at: Some("2026-08-30T08:00:00Z".into()),
                authenticated_sender_domain: Some("example.test".into()),
                body_text: "Your reservation is confirmed. PNR: CANCEL42".into(),
            },
            stale.clone(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(stale_outcome["amendment_ignored"], true);
        assert_eq!(stale_outcome["needs_review"], false);

        // Even a newer, fully structured message cannot amend the booking
        // when Gmail authenticated a different sender domain.
        let forged = ParsedTravelDocument {
            kind: "flight".into(),
            title: "Forged reinstatement".into(),
            start_at: Some("2026-09-10T08:00:00Z".into()),
            end_at: Some("2026-09-10T16:00:00Z".into()),
            confidence: 0.99,
            decision: "auto_accepted".into(),
            ..stale
        };
        let forged_outcome = persist_parsed_document(
            &state,
            TravelSource {
                account_id: "gmail".into(),
                folder: "INBOX".into(),
                uidvalidity: 7,
                uid: 12,
                message_id: Some("forged-reinstatement@example.test".into()),
                from_addr: "Air Test <tickets@evil.test>".into(),
                subject: "Your reservation is confirmed".into(),
                received_at: Some("2026-09-02T08:00:00Z".into()),
                authenticated_sender_domain: Some("evil.test".into()),
                body_text: "Your reservation is confirmed. PNR: CANCEL42".into(),
            },
            forged,
            false,
        )
        .await
        .unwrap();
        assert_eq!(forged_outcome["needs_review"], true);
        assert_eq!(forged_outcome["receipt"]["status"], "quarantined");

        let db = state.db.lock().await;
        let trip = db.list_trips().unwrap().remove(0);
        let booking = db.list_travel_bookings(&trip.id).unwrap().remove(0);
        assert_eq!(booking.status, "cancelled");
        assert_eq!(booking.title, "Flight JFK to CDG");
        assert_eq!(booking.starts_at.as_deref(), Some("2026-09-10T08:00:00Z"));
        assert_eq!(booking.ends_at.as_deref(), Some("2026-09-10T16:00:00Z"));
        let segment = db.list_travel_segments(&trip.id).unwrap().remove(0);
        assert_eq!(segment.status, "cancelled");
        assert_eq!(segment.origin.as_deref(), Some("JFK"));
        assert_eq!(segment.destination.as_deref(), Some("CDG"));
        assert_eq!(segment.departs_at.as_deref(), Some("2026-09-10T08:00:00Z"));
        let receipts = db.list_travel_receipts(None, true, 10).unwrap();
        assert_eq!(
            receipts
                .iter()
                .filter(|receipt| receipt.status == "processed")
                .count(),
            3
        );
        assert_eq!(
            receipts
                .iter()
                .filter(|receipt| receipt.status == "quarantined")
                .count(),
            1
        );
        let alerts = db.list_travel_alerts(Some(&trip.id)).unwrap();
        assert!(alerts.iter().any(|alert| alert.kind == "booking_cancelled"));
        assert!(
            !alerts
                .iter()
                .any(|alert| matches!(alert.kind.as_str(), "check_in" | "upcoming"))
        );
    }

    #[tokio::test]
    async fn gmail_onboarding_is_disabled_on_direct_network_listener() {
        let state = AppState::new(
            envelope_email_store::Database::open_memory().unwrap(),
            envelope_email_store::CredentialBackend::File,
        )
        .with_travel_secret_onboarding(false);
        let response = connect_gmail(
            State(state),
            HeaderMap::new(),
            Json(ConnectGmailRequest {
                email: "traveler@gmail.com".into(),
                password: "should-never-be-probed".into(),
                display_name: None,
                settings: None,
                imap_host: None,
                imap_port: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn gmail_onboarding_requires_loopback_or_https_proxy_transport() {
        let state = AppState::new(
            envelope_email_store::Database::open_memory().unwrap(),
            envelope_email_store::CredentialBackend::File,
        );

        let mut local = HeaderMap::new();
        local.insert(header::HOST, HeaderValue::from_static("127.0.0.1:3141"));
        assert!(gmail_onboarding_transport_is_secure(&state, &local));

        let mut plaintext_proxy = HeaderMap::new();
        plaintext_proxy.insert(
            header::HOST,
            HeaderValue::from_static("travel.example.test"),
        );
        assert!(!gmail_onboarding_transport_is_secure(
            &state,
            &plaintext_proxy
        ));

        let mut https_proxy = plaintext_proxy.clone();
        https_proxy.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        https_proxy.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://travel.example.test"),
        );
        assert!(gmail_onboarding_transport_is_secure(&state, &https_proxy));

        https_proxy.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://travel.example.test"),
        );
        assert!(!gmail_onboarding_transport_is_secure(&state, &https_proxy));

        let missing_host = HeaderMap::new();
        assert!(!gmail_onboarding_transport_is_secure(&state, &missing_host));
    }
}
