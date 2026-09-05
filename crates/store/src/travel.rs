// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Local-first persistence for Envelope Travel.
//!
//! The domain is deliberately single-user: settings are a singleton and trips
//! are not partitioned by an Envelope account. Email ingest identity is still
//! account-scoped because one person may scan more than one mailbox. Callers
//! supply every domain id and timestamp, which makes import retries and tests
//! reproducible. The one exception is a public share bearer token: it must be
//! unpredictable, is generated from OS randomness, returned once, and only its
//! SHA-256 digest is stored.

use std::fmt::Write as _;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::accounts::upsert_account_credentials_on;
use crate::db::Database;
use crate::errors::{Result, StoreError};
use crate::models::Account;

const TRIP_COLUMNS: &str = "id, title, destination, starts_at, ends_at, timezone, status, \
    notes, created_at, updated_at";
const RECEIPT_COLUMNS: &str = "id, account_id, folder, uidvalidity, uid, message_id, \
    content_hash, from_addr, subject, received_at, authenticated_sender_domain, body_text, \
    extracted_json, trip_id, status, quarantine_reason, quarantined_at, created_at, updated_at";
const BOOKING_COLUMNS: &str = "id, trip_id, receipt_id, kind, provider, title, \
    confirmation_code, status, starts_at, ends_at, location, details_json, created_at, updated_at";
const SEGMENT_COLUMNS: &str = "id, trip_id, booking_id, kind, sequence, origin, destination, \
    carrier, service_number, departs_at, arrives_at, status, details_json, created_at, updated_at";
const TASK_COLUMNS: &str =
    "id, trip_id, title, notes, due_at, completed_at, created_at, updated_at";
const ALERT_COLUMNS: &str = "id, trip_id, booking_id, segment_id, dedupe_key, kind, severity, \
    title, body, scheduled_at, triggered_at, acknowledged_at, created_at, updated_at";
const SHARE_COLUMNS: &str = "id, trip_id, token_prefix, label, can_edit_tasks, expires_at, \
    revoked_at, last_accessed_at, created_at";
const INGEST_FAILURE_COLUMNS: &str = "account_id, folder, uidvalidity, uid, stage, attempt_count, \
    last_error, first_failed_at, last_failed_at, quarantined_receipt_id, quarantined_at";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TravelSettings {
    pub account_id: Option<String>,
    pub scan_folder: String,
    pub home_timezone: String,
    pub calendar_name: String,
    pub auto_ingest: bool,
    pub default_alert_minutes: i64,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TravelIngestCursor {
    pub account_id: String,
    pub folder: String,
    pub uidvalidity: i64,
    pub last_uid: i64,
    pub last_message_at: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TravelMessageSourceKey {
    pub account_id: String,
    pub folder: String,
    pub uidvalidity: i64,
    pub uid: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TravelIngestFailure {
    pub source: TravelMessageSourceKey,
    pub stage: String,
    pub attempt_count: i64,
    pub last_error: String,
    pub first_failed_at: String,
    pub last_failed_at: String,
    pub quarantined_receipt_id: Option<String>,
    pub quarantined_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewTravelIngestFailure {
    pub source: TravelMessageSourceKey,
    pub stage: String,
    pub last_error: String,
    pub now: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trip {
    pub id: String,
    pub title: String,
    pub destination: Option<String>,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub timezone: String,
    pub status: String,
    pub notes: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewTrip {
    pub id: String,
    pub title: String,
    pub destination: Option<String>,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub timezone: String,
    pub status: String,
    pub notes: Option<String>,
    /// Used for both `created_at` and `updated_at`.
    pub now: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TripUpdate {
    pub title: String,
    pub destination: Option<String>,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub timezone: String,
    pub status: String,
    pub notes: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TravelReceipt {
    pub id: String,
    pub account_id: String,
    pub folder: String,
    pub uidvalidity: i64,
    pub uid: i64,
    pub message_id: Option<String>,
    pub content_hash: String,
    pub from_addr: Option<String>,
    pub subject: Option<String>,
    pub received_at: Option<String>,
    /// Authenticated RFC5322.From domain, derived only from a trusted Gmail
    /// Authentication-Results header. Never exposed by public travel views.
    pub authenticated_sender_domain: Option<String>,
    pub body_text: Option<String>,
    pub extracted: Option<Value>,
    pub trip_id: Option<String>,
    pub status: String,
    pub quarantine_reason: Option<String>,
    pub quarantined_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewTravelReceipt {
    pub id: String,
    pub account_id: String,
    pub folder: String,
    pub uidvalidity: i64,
    pub uid: i64,
    pub message_id: Option<String>,
    pub from_addr: Option<String>,
    pub subject: Option<String>,
    pub received_at: Option<String>,
    pub authenticated_sender_domain: Option<String>,
    pub body_text: Option<String>,
    pub extracted: Option<Value>,
    pub trip_id: Option<String>,
    /// A non-empty reason creates the row directly in quarantine.
    pub quarantine_reason: Option<String>,
    /// Used for all ingest/quarantine timestamps on a newly inserted row.
    pub ingested_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptDedupeKey {
    MailboxUid,
    MessageId,
    ContentHash,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TravelReceiptIngestResult {
    pub receipt: TravelReceipt,
    pub inserted: bool,
    pub deduped_by: Option<ReceiptDedupeKey>,
}

/// Result of a compare-and-swap transition into a terminal receipt state.
/// Only `pending` and `quarantined` rows are eligible. A terminal row is
/// returned as `Conflict` so callers can distinguish it from a missing id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", content = "receipt", rename_all = "snake_case")]
pub enum TravelReceiptTerminalTransition {
    Applied(TravelReceipt),
    Conflict(TravelReceipt),
    NotFound,
}

/// Result of atomically moving a receipt into owner review and creating its
/// durable review alert. Terminal receipts conflict and are never resurrected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", content = "receipt", rename_all = "snake_case")]
pub enum TravelReceiptReviewTransition {
    Applied(TravelReceipt),
    Conflict(TravelReceipt),
    NotFound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TravelReceiptSourceOrder {
    Older,
    Same,
    Newer,
    Incomparable,
}

/// Compare two receipt sources without treating UIDs from different mailbox
/// epochs or accounts as globally ordered. Within one Gmail UID epoch, UID is
/// the authoritative arrival order. Across scopes, two valid source dates are
/// comparable; otherwise automatic mutation must fail closed.
pub fn compare_travel_receipt_sources(
    candidate: &TravelReceipt,
    current: &TravelReceipt,
) -> TravelReceiptSourceOrder {
    use std::cmp::Ordering;

    let same_epoch = candidate.account_id == current.account_id
        && candidate.folder == current.folder
        && candidate.uidvalidity > 0
        && candidate.uidvalidity == current.uidvalidity;
    if same_epoch {
        return match candidate.uid.cmp(&current.uid) {
            Ordering::Less => TravelReceiptSourceOrder::Older,
            Ordering::Equal => TravelReceiptSourceOrder::Same,
            Ordering::Greater => TravelReceiptSourceOrder::Newer,
        };
    }

    let candidate_time = candidate
        .received_at
        .as_deref()
        .and_then(parse_source_timestamp);
    let current_time = current
        .received_at
        .as_deref()
        .and_then(parse_source_timestamp);
    match (candidate_time, current_time) {
        (Some(candidate), Some(current)) if candidate < current => TravelReceiptSourceOrder::Older,
        (Some(candidate), Some(current)) if candidate > current => TravelReceiptSourceOrder::Newer,
        _ => TravelReceiptSourceOrder::Incomparable,
    }
}

fn parse_source_timestamp(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&chrono::Utc))
        .or_else(|| {
            chrono::DateTime::parse_from_rfc2822(value)
                .ok()
                .map(|value| value.with_timezone(&chrono::Utc))
        })
}

/// Result of an atomic receipt-derived write. The caller-supplied value is
/// returned only when every derived mutation and the final `processed`
/// transition commit together.
#[derive(Debug, Clone, PartialEq)]
pub enum TravelReceiptAtomicTransition<T> {
    Applied { receipt: TravelReceipt, value: T },
    Conflict(TravelReceipt),
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TravelBooking {
    pub id: String,
    pub trip_id: String,
    pub receipt_id: Option<String>,
    pub kind: String,
    pub provider: Option<String>,
    pub title: String,
    pub confirmation_code: Option<String>,
    pub status: String,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub location: Option<String>,
    pub details: Option<Value>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewTravelBooking {
    pub id: String,
    pub trip_id: String,
    pub receipt_id: Option<String>,
    pub kind: String,
    pub provider: Option<String>,
    pub title: String,
    pub confirmation_code: Option<String>,
    pub status: String,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub location: Option<String>,
    pub details: Option<Value>,
    pub now: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TravelBookingUpdate {
    pub receipt_id: Option<String>,
    pub kind: String,
    pub provider: Option<String>,
    pub title: String,
    pub confirmation_code: Option<String>,
    pub status: String,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub location: Option<String>,
    pub details: Option<Value>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TravelSegment {
    pub id: String,
    pub trip_id: String,
    pub booking_id: Option<String>,
    pub kind: String,
    pub sequence: i64,
    pub origin: Option<String>,
    pub destination: Option<String>,
    pub carrier: Option<String>,
    pub service_number: Option<String>,
    pub departs_at: Option<String>,
    pub arrives_at: Option<String>,
    pub status: String,
    pub details: Option<Value>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewTravelSegment {
    pub id: String,
    pub trip_id: String,
    pub booking_id: Option<String>,
    pub kind: String,
    pub sequence: i64,
    pub origin: Option<String>,
    pub destination: Option<String>,
    pub carrier: Option<String>,
    pub service_number: Option<String>,
    pub departs_at: Option<String>,
    pub arrives_at: Option<String>,
    pub status: String,
    pub details: Option<Value>,
    pub now: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TravelSegmentUpdate {
    pub booking_id: Option<String>,
    pub kind: String,
    pub sequence: i64,
    pub origin: Option<String>,
    pub destination: Option<String>,
    pub carrier: Option<String>,
    pub service_number: Option<String>,
    pub departs_at: Option<String>,
    pub arrives_at: Option<String>,
    pub status: String,
    pub details: Option<Value>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TravelTask {
    pub id: String,
    pub trip_id: Option<String>,
    pub title: String,
    pub notes: Option<String>,
    pub due_at: Option<String>,
    pub completed_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewTravelTask {
    pub id: String,
    pub trip_id: Option<String>,
    pub title: String,
    pub notes: Option<String>,
    pub due_at: Option<String>,
    pub now: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TravelTaskUpdate {
    pub trip_id: Option<String>,
    pub title: String,
    pub notes: Option<String>,
    pub due_at: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TravelAlert {
    pub id: String,
    pub trip_id: Option<String>,
    pub booking_id: Option<String>,
    pub segment_id: Option<String>,
    pub dedupe_key: String,
    pub kind: String,
    pub severity: String,
    pub title: String,
    pub body: Option<String>,
    pub scheduled_at: Option<String>,
    pub triggered_at: Option<String>,
    pub acknowledged_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewTravelAlert {
    pub id: String,
    pub trip_id: Option<String>,
    pub booking_id: Option<String>,
    pub segment_id: Option<String>,
    /// Stable semantic key such as `flight:AA100:2026-09-01:gate-change`.
    pub dedupe_key: String,
    pub kind: String,
    pub severity: String,
    pub title: String,
    pub body: Option<String>,
    pub scheduled_at: Option<String>,
    pub now: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TravelAlertEnqueueResult {
    pub alert: TravelAlert,
    pub inserted: bool,
}

/// Stored share metadata. It never contains a raw token or token hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TravelShareLink {
    pub id: String,
    pub trip_id: String,
    pub token_prefix: String,
    pub label: Option<String>,
    pub can_edit_tasks: bool,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
    pub last_accessed_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewTravelShareLink {
    pub id: String,
    pub trip_id: String,
    pub label: Option<String>,
    pub can_edit_tasks: bool,
    pub expires_at: Option<String>,
    pub created_at: String,
}

/// Returned once when a share link is minted. `token` is never persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedTravelShareLink {
    pub link: TravelShareLink,
    pub token: String,
    pub calendar_token: String,
}

/// Public-safe trip data. Private trip notes are intentionally absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicTrip {
    pub id: String,
    pub title: String,
    pub destination: Option<String>,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub timezone: String,
    pub status: String,
    pub updated_at: String,
}

/// Public-safe booking data. Confirmation codes, extracted data, and receipt
/// bodies are intentionally impossible to serialize through this type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicTravelBooking {
    pub id: String,
    pub kind: String,
    pub provider: Option<String>,
    pub title: String,
    pub status: String,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub location: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicTravelSegment {
    pub id: String,
    pub booking_id: Option<String>,
    pub kind: String,
    pub sequence: i64,
    pub origin: Option<String>,
    pub destination: Option<String>,
    pub carrier: Option<String>,
    pub service_number: Option<String>,
    pub departs_at: Option<String>,
    pub arrives_at: Option<String>,
    pub status: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicTravelTask {
    pub id: String,
    pub title: String,
    pub due_at: Option<String>,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicTravelAlert {
    pub id: String,
    pub kind: String,
    pub severity: String,
    pub title: String,
    pub scheduled_at: Option<String>,
    pub triggered_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicTripView {
    pub share_id: String,
    pub can_edit_tasks: bool,
    pub trip: PublicTrip,
    pub bookings: Vec<PublicTravelBooking>,
    pub segments: Vec<PublicTravelSegment>,
    pub tasks: Vec<PublicTravelTask>,
    pub alerts: Vec<PublicTravelAlert>,
}

impl Database {
    // ── Settings and durable mailbox cursors ──────────────────────

    pub fn get_travel_settings(&self) -> Result<Option<TravelSettings>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT account_id, scan_folder, home_timezone, calendar_name,
                        auto_ingest, default_alert_minutes, updated_at
                 FROM travel_settings WHERE id = 1",
                [],
                map_settings,
            )
            .optional()?)
    }

    /// Dashboard-friendly list form of the singleton settings record. The
    /// result contains either zero rows (not configured) or exactly one row.
    pub fn list_travel_settings(&self) -> Result<Vec<TravelSettings>> {
        Ok(self.get_travel_settings()?.into_iter().collect())
    }

    pub fn upsert_travel_settings(&self, settings: &TravelSettings) -> Result<TravelSettings> {
        upsert_travel_settings_on(self.conn(), settings)
    }

    /// Atomically persist a verified Gmail app password and select that account
    /// for Travel. `settings.account_id` is replaced with the created or updated
    /// account id so callers never need to predict an id before the transaction.
    /// Neither write is visible if credential encryption or settings persistence
    /// fails.
    pub fn upsert_gmail_account_and_travel_settings(
        &self,
        name: &str,
        username: &str,
        app_password: &str,
        passphrase: &str,
        settings: &TravelSettings,
    ) -> Result<(Account, TravelSettings)> {
        self.upsert_imap_account_and_travel_settings(
            name,
            username,
            app_password,
            passphrase,
            settings,
            "imap.gmail.com",
            993,
        )
    }

    pub fn upsert_imap_account_and_travel_settings(
        &self,
        name: &str,
        username: &str,
        password: &str,
        passphrase: &str,
        settings: &TravelSettings,
        host: &str,
        port: u16,
    ) -> Result<(Account, TravelSettings)> {
        let tx = self.conn().unchecked_transaction()?;
        let account = upsert_account_credentials_on(
            &tx,
            name,
            username,
            password,
            None,
            "smtp.gmail.com",
            587,
            host,
            port,
            passphrase,
        )?;
        let mut settings = settings.clone();
        settings.account_id = Some(account.id.clone());
        let settings = upsert_travel_settings_on(&tx, &settings)?;
        tx.commit()?;
        Ok((account, settings))
    }

    pub fn get_travel_cursor(
        &self,
        account_id: &str,
        folder: &str,
    ) -> Result<Option<TravelIngestCursor>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT account_id, folder, uidvalidity, last_uid, last_message_at, updated_at
                 FROM travel_ingest_cursors WHERE account_id = ?1 AND folder = ?2",
                params![account_id, folder],
                map_cursor,
            )
            .optional()?)
    }

    /// Commit the cursor after all receipt writes in the caller's scan batch
    /// have succeeded. Identity always includes account, folder, UIDVALIDITY,
    /// and UID; a UIDVALIDITY change replaces the old epoch boundary.
    pub fn upsert_travel_cursor(&self, cursor: &TravelIngestCursor) -> Result<TravelIngestCursor> {
        self.conn().execute(
            "INSERT INTO travel_ingest_cursors
                (account_id, folder, uidvalidity, last_uid, last_message_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(account_id, folder) DO UPDATE SET
                uidvalidity = excluded.uidvalidity,
                last_uid = CASE
                    WHEN travel_ingest_cursors.uidvalidity = excluded.uidvalidity
                    THEN MAX(travel_ingest_cursors.last_uid, excluded.last_uid)
                    ELSE excluded.last_uid
                END,
                last_message_at = excluded.last_message_at,
                updated_at = excluded.updated_at",
            params![
                cursor.account_id,
                cursor.folder,
                cursor.uidvalidity,
                cursor.last_uid,
                cursor.last_message_at,
                cursor.updated_at,
            ],
        )?;
        self.get_travel_cursor(&cursor.account_id, &cursor.folder)?
            .ok_or_else(|| StoreError::Config("travel cursor disappeared after upsert".into()))
    }

    pub fn list_travel_cursors(&self) -> Result<Vec<TravelIngestCursor>> {
        let mut stmt = self.conn().prepare(
            "SELECT account_id, folder, uidvalidity, last_uid, last_message_at, updated_at
             FROM travel_ingest_cursors ORDER BY account_id ASC, folder ASC",
        )?;
        let rows = stmt.query_map([], map_cursor)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn delete_travel_cursor(&self, account_id: &str, folder: &str) -> Result<bool> {
        Ok(self.conn().execute(
            "DELETE FROM travel_ingest_cursors WHERE account_id = ?1 AND folder = ?2",
            params![account_id, folder],
        )? > 0)
    }

    // ── Bounded per-message ingest failures ─────────────────────

    pub fn record_travel_ingest_failure(
        &self,
        input: &NewTravelIngestFailure,
    ) -> Result<TravelIngestFailure> {
        self.conn().execute(
            "INSERT INTO travel_ingest_failures
                (account_id, folder, uidvalidity, uid, stage, attempt_count, last_error,
                 first_failed_at, last_failed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?7)
             ON CONFLICT(account_id, folder, uidvalidity, uid) DO UPDATE SET
                stage = CASE WHEN quarantined_at IS NULL THEN excluded.stage ELSE stage END,
                attempt_count = CASE WHEN quarantined_at IS NULL
                    THEN attempt_count + 1 ELSE attempt_count END,
                last_error = CASE WHEN quarantined_at IS NULL
                    THEN excluded.last_error ELSE last_error END,
                last_failed_at = CASE WHEN quarantined_at IS NULL
                    THEN excluded.last_failed_at ELSE last_failed_at END",
            params![
                input.source.account_id,
                input.source.folder,
                input.source.uidvalidity,
                input.source.uid,
                input.stage,
                input.last_error,
                input.now,
            ],
        )?;
        self.get_travel_ingest_failure(&input.source)?
            .ok_or_else(|| {
                StoreError::Config(format!(
                    "travel ingest failure disappeared after record: {}/{}/{}/{}",
                    input.source.account_id,
                    input.source.folder,
                    input.source.uidvalidity,
                    input.source.uid
                ))
            })
    }

    /// Atomically turn an exhausted per-message retry into an owner-visible
    /// quarantined receipt, its review alert, and the durable skip marker.
    /// A crash cannot leave a handled receipt that suppresses the next scan
    /// before its alert or failure marker has committed.
    pub fn finalize_travel_ingest_failure_quarantine(
        &self,
        source: &TravelMessageSourceKey,
        receipt: &NewTravelReceipt,
        alert: &NewTravelAlert,
        now: &str,
    ) -> Result<TravelReceiptIngestResult> {
        if receipt.account_id != source.account_id
            || receipt.folder != source.folder
            || receipt.uidvalidity != source.uidvalidity
            || receipt.uid != source.uid
        {
            return Err(StoreError::Config(
                "travel ingest quarantine receipt does not match its source".into(),
            ));
        }
        if receipt
            .quarantine_reason
            .as_deref()
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .is_none()
        {
            return Err(StoreError::Config(
                "travel ingest quarantine requires a review reason".into(),
            ));
        }

        let tx = self.conn().unchecked_transaction()?;
        let ingest = ingest_travel_receipt_tx(&tx, receipt)?;
        self.enqueue_travel_alert(alert)?;
        let changed = tx.execute(
            "UPDATE travel_ingest_failures
             SET quarantined_receipt_id = ?5, quarantined_at = COALESCE(quarantined_at, ?6),
                 last_failed_at = ?6
             WHERE account_id = ?1 AND folder = ?2 AND uidvalidity = ?3 AND uid = ?4",
            params![
                source.account_id,
                source.folder,
                source.uidvalidity,
                source.uid,
                ingest.receipt.id,
                now,
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::Config(
                "travel ingest failure disappeared before quarantine".into(),
            ));
        }
        tx.commit()?;
        Ok(ingest)
    }

    pub fn get_travel_ingest_failure(
        &self,
        source: &TravelMessageSourceKey,
    ) -> Result<Option<TravelIngestFailure>> {
        let sql = format!(
            "SELECT {INGEST_FAILURE_COLUMNS} FROM travel_ingest_failures
             WHERE account_id = ?1 AND folder = ?2 AND uidvalidity = ?3 AND uid = ?4"
        );
        Ok(self
            .conn()
            .query_row(
                &sql,
                params![
                    source.account_id,
                    source.folder,
                    source.uidvalidity,
                    source.uid
                ],
                map_ingest_failure,
            )
            .optional()?)
    }

    pub fn list_quarantined_travel_ingest_sources(
        &self,
        account_id: &str,
        folder: &str,
        uidvalidity: i64,
    ) -> Result<Vec<TravelMessageSourceKey>> {
        let mut statement = self.conn().prepare(
            "SELECT account_id, folder, uidvalidity, uid
             FROM travel_ingest_failures
             WHERE account_id = ?1 AND folder = ?2 AND uidvalidity = ?3
               AND quarantined_at IS NOT NULL
             ORDER BY uid ASC",
        )?;
        let rows = statement.query_map(params![account_id, folder, uidvalidity], |row| {
            Ok(TravelMessageSourceKey {
                account_id: row.get(0)?,
                folder: row.get(1)?,
                uidvalidity: row.get(2)?,
                uid: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Sources already represented by a terminal receipt do not need another
    /// raw IMAP fetch during the newest-mail safety sweep. Pending and ordinary
    /// quarantined receipts are intentionally excluded so crash recovery and
    /// newly resolvable amendments can replay. Permanently unreadable sources
    /// are skipped separately through `travel_ingest_failures`.
    pub fn list_handled_travel_receipt_sources(
        &self,
        account_id: &str,
        folder: &str,
        uidvalidity: i64,
    ) -> Result<Vec<TravelMessageSourceKey>> {
        let mut statement = self.conn().prepare(
            "SELECT account_id, folder, uidvalidity, uid
             FROM travel_receipts
             WHERE account_id = ?1 AND folder = ?2 AND uidvalidity = ?3
               AND status IN ('processed', 'dismissed')
             ORDER BY uid ASC",
        )?;
        let rows = statement.query_map(params![account_id, folder, uidvalidity], |row| {
            Ok(TravelMessageSourceKey {
                account_id: row.get(0)?,
                folder: row.get(1)?,
                uidvalidity: row.get(2)?,
                uid: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn mark_travel_ingest_failure_quarantined(
        &self,
        source: &TravelMessageSourceKey,
        receipt_id: &str,
        now: &str,
    ) -> Result<Option<TravelIngestFailure>> {
        let changed = self.conn().execute(
            "UPDATE travel_ingest_failures
             SET quarantined_receipt_id = ?5, quarantined_at = COALESCE(quarantined_at, ?6),
                 last_failed_at = ?6
             WHERE account_id = ?1 AND folder = ?2 AND uidvalidity = ?3 AND uid = ?4",
            params![
                source.account_id,
                source.folder,
                source.uidvalidity,
                source.uid,
                receipt_id,
                now,
            ],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_travel_ingest_failure(source)
    }

    /// Clear a transient failure after the source message is read and handled.
    /// Quarantined rows remain as durable skip markers for bounded rescans.
    pub fn clear_open_travel_ingest_failure(
        &self,
        source: &TravelMessageSourceKey,
    ) -> Result<bool> {
        Ok(self.conn().execute(
            "DELETE FROM travel_ingest_failures
             WHERE account_id = ?1 AND folder = ?2 AND uidvalidity = ?3 AND uid = ?4
               AND quarantined_at IS NULL",
            params![
                source.account_id,
                source.folder,
                source.uidvalidity,
                source.uid
            ],
        )? > 0)
    }

    // ── Trips ─────────────────────────────────────────────────

    pub fn create_trip(&self, input: &NewTrip) -> Result<Trip> {
        self.conn().execute(
            "INSERT INTO travel_trips
                (id, title, destination, starts_at, ends_at, timezone, status, notes,
                 created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                input.id,
                input.title,
                input.destination,
                input.starts_at,
                input.ends_at,
                input.timezone,
                input.status,
                input.notes,
                input.now,
            ],
        )?;
        required(self.get_trip(&input.id)?, "trip", &input.id)
    }

    pub fn get_trip(&self, id: &str) -> Result<Option<Trip>> {
        let sql = format!("SELECT {TRIP_COLUMNS} FROM travel_trips WHERE id = ?1");
        Ok(self
            .conn()
            .query_row(&sql, params![id], map_trip)
            .optional()?)
    }

    pub fn list_trips(&self) -> Result<Vec<Trip>> {
        let sql = format!(
            "SELECT {TRIP_COLUMNS} FROM travel_trips
             ORDER BY CASE WHEN starts_at IS NULL THEN 1 ELSE 0 END,
                      starts_at ASC, created_at DESC, id ASC"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map([], map_trip)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn update_trip(&self, id: &str, update: &TripUpdate) -> Result<Option<Trip>> {
        let changed = self.conn().execute(
            "UPDATE travel_trips SET title = ?2, destination = ?3, starts_at = ?4,
                    ends_at = ?5, timezone = ?6, status = ?7, notes = ?8, updated_at = ?9
             WHERE id = ?1",
            params![
                id,
                update.title,
                update.destination,
                update.starts_at,
                update.ends_at,
                update.timezone,
                update.status,
                update.notes,
                update.updated_at,
            ],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_trip(id)
    }

    /// Delete a trip and its derived children transactionally. Receipt source
    /// records are retained and merely unassigned. This is explicit because
    /// existing Envelope connections do not globally enable foreign keys.
    pub fn delete_trip(&self, id: &str) -> Result<bool> {
        let tx = self.conn().unchecked_transaction()?;
        tx.execute("DELETE FROM travel_alerts WHERE trip_id = ?1", params![id])?;
        tx.execute(
            "DELETE FROM travel_alerts WHERE booking_id IN
                (SELECT id FROM travel_bookings WHERE trip_id = ?1)
               OR segment_id IN (SELECT id FROM travel_segments WHERE trip_id = ?1)",
            params![id],
        )?;
        tx.execute(
            "DELETE FROM travel_segments WHERE trip_id = ?1",
            params![id],
        )?;
        tx.execute(
            "DELETE FROM travel_bookings WHERE trip_id = ?1",
            params![id],
        )?;
        tx.execute("DELETE FROM travel_tasks WHERE trip_id = ?1", params![id])?;
        tx.execute(
            "DELETE FROM travel_share_links WHERE trip_id = ?1",
            params![id],
        )?;
        tx.execute(
            "UPDATE travel_receipts SET trip_id = NULL WHERE trip_id = ?1",
            params![id],
        )?;
        let deleted = tx.execute("DELETE FROM travel_trips WHERE id = ?1", params![id])? > 0;
        tx.commit()?;
        Ok(deleted)
    }

    // ── Receipt ingest and quarantine ─────────────────────────

    pub fn ingest_travel_receipt(
        &self,
        input: &NewTravelReceipt,
    ) -> Result<TravelReceiptIngestResult> {
        let tx = self.conn().unchecked_transaction()?;
        let ingest = ingest_travel_receipt_tx(&tx, input)?;
        tx.commit()?;
        Ok(ingest)
    }

    pub fn get_travel_receipt(&self, id: &str) -> Result<Option<TravelReceipt>> {
        let sql = format!("SELECT {RECEIPT_COLUMNS} FROM travel_receipts WHERE id = ?1");
        Ok(self
            .conn()
            .query_row(&sql, params![id], map_receipt)
            .optional()?)
    }

    pub fn list_travel_receipts(
        &self,
        trip_id: Option<&str>,
        include_quarantined: bool,
        limit: usize,
    ) -> Result<Vec<TravelReceipt>> {
        let sql = format!(
            "SELECT {RECEIPT_COLUMNS} FROM travel_receipts
             WHERE (?1 IS NULL OR trip_id = ?1)
               AND (?2 = 1 OR status != 'quarantined')
             ORDER BY COALESCE(received_at, created_at) DESC, id ASC LIMIT ?3"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(
            params![trip_id, include_quarantined, limit as i64],
            map_receipt,
        )?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn list_quarantined_travel_receipts(&self, limit: usize) -> Result<Vec<TravelReceipt>> {
        let sql = format!(
            "SELECT {RECEIPT_COLUMNS} FROM travel_receipts
             WHERE status = 'quarantined'
             ORDER BY COALESCE(quarantined_at, created_at) DESC, id ASC LIMIT ?1"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(params![limit as i64], map_receipt)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn quarantine_travel_receipt(
        &self,
        id: &str,
        reason: &str,
        now: &str,
    ) -> Result<Option<TravelReceipt>> {
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(StoreError::Config(
                "travel receipt quarantine reason cannot be empty".into(),
            ));
        }
        let changed = self.conn().execute(
            "UPDATE travel_receipts SET status = 'quarantined', quarantine_reason = ?2,
                    quarantined_at = ?3, updated_at = ?3
             WHERE id = ?1 AND status IN ('pending', 'quarantined')",
            params![id, reason, now],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_travel_receipt(id)
    }

    /// Quarantine a receipt and create its idempotent owner-review alert in
    /// one transaction. A failure in either write leaves the receipt in its
    /// prior state so replay can safely retry the complete operation.
    pub fn quarantine_travel_receipt_with_review_alert(
        &self,
        id: &str,
        reason: &str,
        alert: &NewTravelAlert,
        now: &str,
    ) -> Result<TravelReceiptReviewTransition> {
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(StoreError::Config(
                "travel receipt quarantine reason cannot be empty".into(),
            ));
        }
        let expected_dedupe_key = format!("receipt:{id}:review");
        if alert.kind != "receipt_review" || alert.dedupe_key != expected_dedupe_key {
            return Err(StoreError::Config(format!(
                "travel review alert identity does not match receipt {id}"
            )));
        }

        let tx = self.conn().unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE travel_receipts SET status = 'quarantined', quarantine_reason = ?2,
                    quarantined_at = COALESCE(quarantined_at, ?3), updated_at = ?3
             WHERE id = ?1 AND status IN ('pending', 'quarantined')",
            params![id, reason, now],
        )?;
        if changed == 0 {
            let outcome = match get_receipt_tx(&tx, id)? {
                Some(receipt) => TravelReceiptReviewTransition::Conflict(receipt),
                None => TravelReceiptReviewTransition::NotFound,
            };
            tx.commit()?;
            return Ok(outcome);
        }
        enqueue_travel_alert_on(&tx, alert)?;
        let receipt = get_receipt_tx(&tx, id)?.ok_or_else(|| {
            StoreError::Config(format!(
                "travel receipt disappeared during review transition: {id}"
            ))
        })?;
        tx.commit()?;
        Ok(TravelReceiptReviewTransition::Applied(receipt))
    }

    pub fn release_travel_receipt(&self, id: &str, now: &str) -> Result<Option<TravelReceipt>> {
        let changed = self.conn().execute(
            "UPDATE travel_receipts SET status = 'pending', quarantine_reason = NULL,
                    quarantined_at = NULL, updated_at = ?2
             WHERE id = ?1 AND status IN ('pending', 'quarantined')",
            params![id, now],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_travel_receipt(id)
    }

    pub fn dismiss_travel_receipt(
        &self,
        id: &str,
        now: &str,
    ) -> Result<TravelReceiptTerminalTransition> {
        let tx = self.conn().unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE travel_receipts SET status = 'dismissed', quarantine_reason = NULL,
                    quarantined_at = NULL, updated_at = ?2
             WHERE id = ?1 AND status IN ('pending', 'quarantined')",
            params![id, now],
        )?;
        if changed == 1 {
            tx.execute(
                "UPDATE travel_alerts
                 SET acknowledged_at = ?2, updated_at = ?2
                 WHERE dedupe_key = ?1 AND acknowledged_at IS NULL",
                params![format!("receipt:{id}:review"), now],
            )?;
        }
        let outcome = terminal_receipt_transition_tx(&tx, id, changed)?;
        tx.commit()?;
        Ok(outcome)
    }

    pub fn mark_travel_receipt_processed(
        &self,
        id: &str,
        trip_id: Option<&str>,
        extracted: Option<&Value>,
        now: &str,
    ) -> Result<TravelReceiptTerminalTransition> {
        let extracted_json = json_to_text(extracted)?;
        let tx = self.conn().unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE travel_receipts SET status = 'processed', trip_id = ?2,
                    extracted_json = ?3, quarantine_reason = NULL, quarantined_at = NULL,
                    updated_at = ?4
             WHERE id = ?1 AND status IN ('pending', 'quarantined')",
            params![id, trip_id, extracted_json, now],
        )?;
        let outcome = terminal_receipt_transition_tx(&tx, id, changed)?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Run all itinerary mutations derived from one receipt and its terminal
    /// `processed` transition in a single SQLite transaction.
    ///
    /// `operation` may call the ordinary non-transaction-opening travel CRUD
    /// methods on the supplied database; because they share this connection,
    /// their writes participate in the outer transaction. It returns the trip
    /// id to associate with the receipt plus an arbitrary caller value. Any
    /// closure error, panic, or failed terminal CAS rolls back every derived
    /// write, leaving a pending/quarantined receipt replayable.
    pub fn process_travel_receipt_atomically<T, F>(
        &self,
        receipt_id: &str,
        extracted: Option<&Value>,
        now: &str,
        operation: F,
    ) -> Result<TravelReceiptAtomicTransition<T>>
    where
        F: FnOnce(&Database) -> Result<(Option<String>, T)>,
    {
        let extracted_json = json_to_text(extracted)?;
        let tx = self.conn().unchecked_transaction()?;
        let Some(current) = get_receipt_tx(&tx, receipt_id)? else {
            tx.commit()?;
            return Ok(TravelReceiptAtomicTransition::NotFound);
        };
        if !matches!(current.status.as_str(), "pending" | "quarantined") {
            tx.commit()?;
            return Ok(TravelReceiptAtomicTransition::Conflict(current));
        }

        let (trip_id, value) = match operation(self) {
            Ok(output) => output,
            Err(error) => {
                tx.rollback()?;
                return Err(error);
            }
        };
        let changed = tx.execute(
            "UPDATE travel_receipts SET status = 'processed', trip_id = ?2,
                    extracted_json = ?3, quarantine_reason = NULL, quarantined_at = NULL,
                    updated_at = ?4
             WHERE id = ?1 AND status IN ('pending', 'quarantined')",
            params![receipt_id, trip_id, extracted_json, now],
        )?;
        let outcome = terminal_receipt_transition_tx(&tx, receipt_id, changed)?;
        match outcome {
            TravelReceiptTerminalTransition::Applied(receipt) => {
                tx.commit()?;
                Ok(TravelReceiptAtomicTransition::Applied { receipt, value })
            }
            TravelReceiptTerminalTransition::Conflict(receipt) => {
                tx.rollback()?;
                Ok(TravelReceiptAtomicTransition::Conflict(receipt))
            }
            TravelReceiptTerminalTransition::NotFound => {
                tx.rollback()?;
                Ok(TravelReceiptAtomicTransition::NotFound)
            }
        }
    }

    pub fn delete_travel_receipt(&self, id: &str) -> Result<bool> {
        let tx = self.conn().unchecked_transaction()?;
        tx.execute(
            "UPDATE travel_bookings SET receipt_id = NULL WHERE receipt_id = ?1",
            params![id],
        )?;
        let deleted = tx.execute("DELETE FROM travel_receipts WHERE id = ?1", params![id])? > 0;
        tx.commit()?;
        Ok(deleted)
    }

    // ── Bookings ───────────────────────────────────────────────

    pub fn create_travel_booking(&self, input: &NewTravelBooking) -> Result<TravelBooking> {
        let details_json = json_to_text(input.details.as_ref())?;
        self.conn().execute(
            "INSERT INTO travel_bookings
                (id, trip_id, receipt_id, kind, provider, title, confirmation_code, status,
                 starts_at, ends_at, location, details_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
            params![
                input.id,
                input.trip_id,
                input.receipt_id,
                input.kind,
                input.provider,
                input.title,
                input.confirmation_code,
                input.status,
                input.starts_at,
                input.ends_at,
                input.location,
                details_json,
                input.now,
            ],
        )?;
        required(
            self.get_travel_booking(&input.id)?,
            "travel booking",
            &input.id,
        )
    }

    pub fn get_travel_booking(&self, id: &str) -> Result<Option<TravelBooking>> {
        let sql = format!("SELECT {BOOKING_COLUMNS} FROM travel_bookings WHERE id = ?1");
        Ok(self
            .conn()
            .query_row(&sql, params![id], map_booking)
            .optional()?)
    }

    pub fn list_travel_bookings(&self, trip_id: &str) -> Result<Vec<TravelBooking>> {
        let sql = format!(
            "SELECT {BOOKING_COLUMNS} FROM travel_bookings WHERE trip_id = ?1
             ORDER BY CASE WHEN starts_at IS NULL THEN 1 ELSE 0 END,
                      starts_at ASC, created_at ASC, id ASC"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(params![trip_id], map_booking)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn update_travel_booking(
        &self,
        id: &str,
        update: &TravelBookingUpdate,
    ) -> Result<Option<TravelBooking>> {
        let details_json = json_to_text(update.details.as_ref())?;
        let changed = self.conn().execute(
            "UPDATE travel_bookings SET receipt_id = ?2, kind = ?3, provider = ?4,
                    title = ?5, confirmation_code = ?6, status = ?7, starts_at = ?8,
                    ends_at = ?9, location = ?10, details_json = ?11, updated_at = ?12
             WHERE id = ?1",
            params![
                id,
                update.receipt_id,
                update.kind,
                update.provider,
                update.title,
                update.confirmation_code,
                update.status,
                update.starts_at,
                update.ends_at,
                update.location,
                details_json,
                update.updated_at,
            ],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_travel_booking(id)
    }

    pub fn delete_travel_booking(&self, id: &str) -> Result<bool> {
        let tx = self.conn().unchecked_transaction()?;
        tx.execute(
            "DELETE FROM travel_alerts WHERE booking_id = ?1 OR segment_id IN
                (SELECT id FROM travel_segments WHERE booking_id = ?1)",
            params![id],
        )?;
        tx.execute(
            "DELETE FROM travel_segments WHERE booking_id = ?1",
            params![id],
        )?;
        let deleted = tx.execute("DELETE FROM travel_bookings WHERE id = ?1", params![id])? > 0;
        tx.commit()?;
        Ok(deleted)
    }

    // ── Segments ───────────────────────────────────────────────

    pub fn create_travel_segment(&self, input: &NewTravelSegment) -> Result<TravelSegment> {
        let details_json = json_to_text(input.details.as_ref())?;
        self.conn().execute(
            "INSERT INTO travel_segments
                (id, trip_id, booking_id, kind, sequence, origin, destination, carrier,
                 service_number, departs_at, arrives_at, status, details_json,
                 created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)",
            params![
                input.id,
                input.trip_id,
                input.booking_id,
                input.kind,
                input.sequence,
                input.origin,
                input.destination,
                input.carrier,
                input.service_number,
                input.departs_at,
                input.arrives_at,
                input.status,
                details_json,
                input.now,
            ],
        )?;
        required(
            self.get_travel_segment(&input.id)?,
            "travel segment",
            &input.id,
        )
    }

    pub fn get_travel_segment(&self, id: &str) -> Result<Option<TravelSegment>> {
        let sql = format!("SELECT {SEGMENT_COLUMNS} FROM travel_segments WHERE id = ?1");
        Ok(self
            .conn()
            .query_row(&sql, params![id], map_segment)
            .optional()?)
    }

    pub fn list_travel_segments(&self, trip_id: &str) -> Result<Vec<TravelSegment>> {
        let sql = format!(
            "SELECT {SEGMENT_COLUMNS} FROM travel_segments WHERE trip_id = ?1
             ORDER BY sequence ASC, departs_at ASC, created_at ASC, id ASC"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(params![trip_id], map_segment)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn update_travel_segment(
        &self,
        id: &str,
        update: &TravelSegmentUpdate,
    ) -> Result<Option<TravelSegment>> {
        let details_json = json_to_text(update.details.as_ref())?;
        let changed = self.conn().execute(
            "UPDATE travel_segments SET booking_id = ?2, kind = ?3, sequence = ?4,
                    origin = ?5, destination = ?6, carrier = ?7, service_number = ?8,
                    departs_at = ?9, arrives_at = ?10, status = ?11, details_json = ?12,
                    updated_at = ?13 WHERE id = ?1",
            params![
                id,
                update.booking_id,
                update.kind,
                update.sequence,
                update.origin,
                update.destination,
                update.carrier,
                update.service_number,
                update.departs_at,
                update.arrives_at,
                update.status,
                details_json,
                update.updated_at,
            ],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_travel_segment(id)
    }

    pub fn delete_travel_segment(&self, id: &str) -> Result<bool> {
        let tx = self.conn().unchecked_transaction()?;
        tx.execute(
            "DELETE FROM travel_alerts WHERE segment_id = ?1",
            params![id],
        )?;
        let deleted = tx.execute("DELETE FROM travel_segments WHERE id = ?1", params![id])? > 0;
        tx.commit()?;
        Ok(deleted)
    }

    // ── Tasks ─────────────────────────────────────────────────────

    pub fn create_travel_task(&self, input: &NewTravelTask) -> Result<TravelTask> {
        self.conn().execute(
            "INSERT INTO travel_tasks
                (id, trip_id, title, notes, due_at, completed_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?6)",
            params![
                input.id,
                input.trip_id,
                input.title,
                input.notes,
                input.due_at,
                input.now,
            ],
        )?;
        required(self.get_travel_task(&input.id)?, "travel task", &input.id)
    }

    pub fn get_travel_task(&self, id: &str) -> Result<Option<TravelTask>> {
        let sql = format!("SELECT {TASK_COLUMNS} FROM travel_tasks WHERE id = ?1");
        Ok(self
            .conn()
            .query_row(&sql, params![id], map_task)
            .optional()?)
    }

    pub fn list_travel_tasks(&self, trip_id: Option<&str>) -> Result<Vec<TravelTask>> {
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM travel_tasks
             WHERE (?1 IS NULL OR trip_id = ?1)
             ORDER BY CASE WHEN completed_at IS NULL THEN 0 ELSE 1 END,
                      CASE WHEN due_at IS NULL THEN 1 ELSE 0 END, due_at ASC, created_at ASC"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(params![trip_id], map_task)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn update_travel_task(
        &self,
        id: &str,
        update: &TravelTaskUpdate,
    ) -> Result<Option<TravelTask>> {
        let changed = self.conn().execute(
            "UPDATE travel_tasks SET trip_id = ?2, title = ?3, notes = ?4, due_at = ?5,
                    updated_at = ?6 WHERE id = ?1",
            params![
                id,
                update.trip_id,
                update.title,
                update.notes,
                update.due_at,
                update.updated_at,
            ],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_travel_task(id)
    }

    pub fn set_travel_task_completed(
        &self,
        id: &str,
        completed: bool,
        now: &str,
    ) -> Result<Option<TravelTask>> {
        let completed_at = completed.then_some(now);
        let changed = self.conn().execute(
            "UPDATE travel_tasks SET completed_at = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, completed_at, now],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_travel_task(id)
    }

    pub fn toggle_travel_task(&self, id: &str, now: &str) -> Result<Option<TravelTask>> {
        let changed = self.conn().execute(
            "UPDATE travel_tasks SET completed_at = CASE
                    WHEN completed_at IS NULL THEN ?2 ELSE NULL END,
                    updated_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_travel_task(id)
    }

    pub fn delete_travel_task(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM travel_tasks WHERE id = ?1", params![id])?
            > 0)
    }

    // ── Alerts ────────────────────────────────────────────────────

    pub fn enqueue_travel_alert(&self, input: &NewTravelAlert) -> Result<TravelAlertEnqueueResult> {
        // Preserve the standalone insert/read atomicity while also allowing
        // receipt processing to call this method inside its encompassing
        // transaction without attempting a nested BEGIN.
        if self.conn().is_autocommit() {
            let tx = self.conn().unchecked_transaction()?;
            let result = enqueue_travel_alert_on(&tx, input)?;
            tx.commit()?;
            Ok(result)
        } else {
            enqueue_travel_alert_on(self.conn(), input)
        }
    }

    pub fn get_travel_alert(&self, id: &str) -> Result<Option<TravelAlert>> {
        let sql = format!("SELECT {ALERT_COLUMNS} FROM travel_alerts WHERE id = ?1");
        Ok(self
            .conn()
            .query_row(&sql, params![id], map_alert)
            .optional()?)
    }

    pub fn list_travel_alerts(&self, trip_id: Option<&str>) -> Result<Vec<TravelAlert>> {
        let sql = format!(
            "SELECT {ALERT_COLUMNS} FROM travel_alerts
             WHERE (?1 IS NULL OR trip_id = ?1)
             ORDER BY CASE WHEN acknowledged_at IS NULL THEN 0 ELSE 1 END,
                      COALESCE(scheduled_at, created_at) ASC, id ASC"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(params![trip_id], map_alert)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn list_due_travel_alerts(&self, now: &str, limit: usize) -> Result<Vec<TravelAlert>> {
        let sql = format!(
            "SELECT {ALERT_COLUMNS} FROM travel_alerts
             WHERE triggered_at IS NULL
               AND datetime(COALESCE(scheduled_at, created_at)) <= datetime(?1)
             ORDER BY COALESCE(scheduled_at, created_at) ASC, id ASC LIMIT ?2"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(params![now, limit as i64], map_alert)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Remove obsolete scheduled reminders for a booking while optionally
    /// preserving the reminder that represents its current start time. This
    /// makes reminder reconciliation idempotent across receipt replays, date
    /// amendments, booking-kind changes, and cancellations.
    pub fn delete_travel_booking_reminders_except(
        &self,
        booking_id: &str,
        keep_dedupe_key: Option<&str>,
    ) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM travel_alerts
             WHERE booking_id = ?1
               AND kind IN ('check_in', 'upcoming')
               AND (?2 IS NULL OR dedupe_key <> ?2)",
            params![booking_id, keep_dedupe_key],
        )?)
    }

    pub fn mark_travel_alert_triggered(&self, id: &str, now: &str) -> Result<Option<TravelAlert>> {
        let changed = self.conn().execute(
            "UPDATE travel_alerts SET triggered_at = COALESCE(triggered_at, ?2),
                    updated_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_travel_alert(id)
    }

    pub fn acknowledge_travel_alert(&self, id: &str, now: &str) -> Result<Option<TravelAlert>> {
        let changed = self.conn().execute(
            "UPDATE travel_alerts SET acknowledged_at = COALESCE(acknowledged_at, ?2),
                    updated_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_travel_alert(id)
    }

    pub fn delete_travel_alert(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM travel_alerts WHERE id = ?1", params![id])?
            > 0)
    }

    // ── Share links and the public-safe aggregate ───────────────────

    pub fn create_travel_share_link(
        &self,
        input: &NewTravelShareLink,
    ) -> Result<CreatedTravelShareLink> {
        let token = generate_share_token();
        let calendar_token = derive_travel_calendar_token(&token);
        let token_hash = hash_share_token(&token);
        let calendar_token_hash = hash_share_token(&calendar_token);
        let token_prefix: String = token.chars().take(15).collect();
        self.conn().execute(
            "INSERT INTO travel_share_links
                (id, trip_id, token_hash, calendar_token_hash, token_prefix, label,
                 can_edit_tasks, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                input.id,
                input.trip_id,
                token_hash,
                calendar_token_hash,
                token_prefix,
                input.label,
                input.can_edit_tasks,
                input.expires_at,
                input.created_at,
            ],
        )?;
        let link = required(
            self.get_travel_share_link(&input.id)?,
            "travel share link",
            &input.id,
        )?;
        Ok(CreatedTravelShareLink {
            link,
            token,
            calendar_token,
        })
    }

    pub fn get_travel_share_link(&self, id: &str) -> Result<Option<TravelShareLink>> {
        let sql = format!("SELECT {SHARE_COLUMNS} FROM travel_share_links WHERE id = ?1");
        Ok(self
            .conn()
            .query_row(&sql, params![id], map_share_link)
            .optional()?)
    }

    pub fn list_travel_share_links(&self, trip_id: &str) -> Result<Vec<TravelShareLink>> {
        let sql = format!(
            "SELECT {SHARE_COLUMNS} FROM travel_share_links WHERE trip_id = ?1
             ORDER BY created_at DESC, id ASC"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(params![trip_id], map_share_link)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Authenticate a raw bearer token and record access in one transaction.
    /// Revoked and expired links fail closed.
    pub fn lookup_travel_share_link(
        &self,
        token: &str,
        now: &str,
    ) -> Result<Option<TravelShareLink>> {
        let token_hash = hash_share_token(token);
        let tx = self.conn().unchecked_transaction()?;
        let sql = format!(
            "SELECT {SHARE_COLUMNS} FROM travel_share_links
             WHERE token_hash = ?1 AND revoked_at IS NULL
               AND (expires_at IS NULL OR datetime(expires_at) > datetime(?2))"
        );
        let mut link = tx
            .query_row(&sql, params![token_hash, now], map_share_link)
            .optional()?;
        if let Some(found) = link.as_mut() {
            tx.execute(
                "UPDATE travel_share_links SET last_accessed_at = ?2 WHERE id = ?1",
                params![found.id, now],
            )?;
            found.last_accessed_at = Some(now.to_string());
        }
        tx.commit()?;
        Ok(link)
    }

    /// Authenticate the independent calendar-only token for a share. Calendar
    /// subscribers never receive the page token that may carry task-edit
    /// authority.
    pub fn lookup_travel_calendar_share_link(
        &self,
        token: &str,
        now: &str,
    ) -> Result<Option<TravelShareLink>> {
        self.lookup_travel_share_link_by_column("calendar_token_hash", token, now)
    }

    fn lookup_travel_share_link_by_column(
        &self,
        column: &str,
        token: &str,
        now: &str,
    ) -> Result<Option<TravelShareLink>> {
        debug_assert!(matches!(column, "token_hash" | "calendar_token_hash"));
        let token_hash = hash_share_token(token);
        let tx = self.conn().unchecked_transaction()?;
        let sql = format!(
            "SELECT {SHARE_COLUMNS} FROM travel_share_links
             WHERE {column} = ?1 AND revoked_at IS NULL
               AND (expires_at IS NULL OR datetime(expires_at) > datetime(?2))"
        );
        let mut link = tx
            .query_row(&sql, params![token_hash, now], map_share_link)
            .optional()?;
        if let Some(found) = link.as_mut() {
            tx.execute(
                "UPDATE travel_share_links SET last_accessed_at = ?2 WHERE id = ?1",
                params![found.id, now],
            )?;
            found.last_accessed_at = Some(now.to_string());
        }
        tx.commit()?;
        Ok(link)
    }

    pub fn revoke_travel_share_link(
        &self,
        id: &str,
        revoked_at: &str,
    ) -> Result<Option<TravelShareLink>> {
        let changed = self.conn().execute(
            "UPDATE travel_share_links SET revoked_at = COALESCE(revoked_at, ?2)
             WHERE id = ?1",
            params![id, revoked_at],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_travel_share_link(id)
    }

    pub fn delete_travel_share_link(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM travel_share_links WHERE id = ?1", params![id])?
            > 0)
    }

    /// Load the family-facing aggregate. Receipt rows, receipt bodies,
    /// extracted JSON, booking confirmation codes/details, trip/task notes,
    /// and alert bodies are structurally omitted.
    pub fn public_trip_view(&self, token: &str, now: &str) -> Result<Option<PublicTripView>> {
        let Some(share) = self.lookup_travel_share_link(token, now)? else {
            return Ok(None);
        };
        self.public_trip_view_for_share(share)
    }

    pub fn public_trip_calendar_view(
        &self,
        token: &str,
        now: &str,
    ) -> Result<Option<PublicTripView>> {
        let Some(share) = self.lookup_travel_calendar_share_link(token, now)? else {
            return Ok(None);
        };
        self.public_trip_view_for_share(share)
    }

    fn public_trip_view_for_share(&self, share: TravelShareLink) -> Result<Option<PublicTripView>> {
        let Some(trip) = self.get_trip(&share.trip_id)? else {
            return Ok(None);
        };
        let bookings = self
            .list_travel_bookings(&trip.id)?
            .into_iter()
            .map(PublicTravelBooking::from)
            .collect();
        let segments = self
            .list_travel_segments(&trip.id)?
            .into_iter()
            .map(PublicTravelSegment::from)
            .collect();
        let tasks = self
            .list_travel_tasks(Some(&trip.id))?
            .into_iter()
            .map(PublicTravelTask::from)
            .collect();
        let alerts = self
            .list_travel_alerts(Some(&trip.id))?
            .into_iter()
            .map(PublicTravelAlert::from)
            .collect();
        Ok(Some(PublicTripView {
            share_id: share.id,
            can_edit_tasks: share.can_edit_tasks,
            trip: PublicTrip::from(trip),
            bookings,
            segments,
            tasks,
            alerts,
        }))
    }
}

impl From<Trip> for PublicTrip {
    fn from(trip: Trip) -> Self {
        Self {
            id: trip.id,
            title: trip.title,
            destination: trip.destination,
            starts_at: trip.starts_at,
            ends_at: trip.ends_at,
            timezone: trip.timezone,
            status: trip.status,
            updated_at: trip.updated_at,
        }
    }
}

impl From<TravelBooking> for PublicTravelBooking {
    fn from(booking: TravelBooking) -> Self {
        Self {
            id: booking.id,
            kind: booking.kind,
            provider: booking.provider,
            title: booking.title,
            status: booking.status,
            starts_at: booking.starts_at,
            ends_at: booking.ends_at,
            location: booking.location,
            updated_at: booking.updated_at,
        }
    }
}

impl From<TravelSegment> for PublicTravelSegment {
    fn from(segment: TravelSegment) -> Self {
        Self {
            id: segment.id,
            booking_id: segment.booking_id,
            kind: segment.kind,
            sequence: segment.sequence,
            origin: segment.origin,
            destination: segment.destination,
            carrier: segment.carrier,
            service_number: segment.service_number,
            departs_at: segment.departs_at,
            arrives_at: segment.arrives_at,
            status: segment.status,
            updated_at: segment.updated_at,
        }
    }
}

impl From<TravelTask> for PublicTravelTask {
    fn from(task: TravelTask) -> Self {
        Self {
            id: task.id,
            title: task.title,
            due_at: task.due_at,
            completed_at: task.completed_at,
        }
    }
}

impl From<TravelAlert> for PublicTravelAlert {
    fn from(alert: TravelAlert) -> Self {
        Self {
            id: alert.id,
            kind: alert.kind,
            severity: alert.severity,
            title: alert.title,
            scheduled_at: alert.scheduled_at,
            triggered_at: alert.triggered_at,
        }
    }
}

fn upsert_travel_settings_on(
    conn: &Connection,
    settings: &TravelSettings,
) -> Result<TravelSettings> {
    conn.execute(
        "INSERT INTO travel_settings
            (id, account_id, scan_folder, home_timezone, calendar_name,
             auto_ingest, default_alert_minutes, updated_at)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET
            account_id = excluded.account_id,
            scan_folder = excluded.scan_folder,
            home_timezone = excluded.home_timezone,
            calendar_name = excluded.calendar_name,
            auto_ingest = excluded.auto_ingest,
            default_alert_minutes = excluded.default_alert_minutes,
            updated_at = excluded.updated_at",
        params![
            settings.account_id,
            settings.scan_folder,
            settings.home_timezone,
            settings.calendar_name,
            settings.auto_ingest,
            settings.default_alert_minutes,
            settings.updated_at,
        ],
    )?;
    conn.query_row(
        "SELECT account_id, scan_folder, home_timezone, calendar_name,
                auto_ingest, default_alert_minutes, updated_at
         FROM travel_settings WHERE id = 1",
        [],
        map_settings,
    )
    .optional()?
    .ok_or_else(|| StoreError::Config("travel settings disappeared after upsert".to_string()))
}

fn map_settings(row: &rusqlite::Row<'_>) -> rusqlite::Result<TravelSettings> {
    Ok(TravelSettings {
        account_id: row.get(0)?,
        scan_folder: row.get(1)?,
        home_timezone: row.get(2)?,
        calendar_name: row.get(3)?,
        auto_ingest: row.get(4)?,
        default_alert_minutes: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn map_cursor(row: &rusqlite::Row<'_>) -> rusqlite::Result<TravelIngestCursor> {
    Ok(TravelIngestCursor {
        account_id: row.get(0)?,
        folder: row.get(1)?,
        uidvalidity: row.get(2)?,
        last_uid: row.get(3)?,
        last_message_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

fn map_trip(row: &rusqlite::Row<'_>) -> rusqlite::Result<Trip> {
    Ok(Trip {
        id: row.get(0)?,
        title: row.get(1)?,
        destination: row.get(2)?,
        starts_at: row.get(3)?,
        ends_at: row.get(4)?,
        timezone: row.get(5)?,
        status: row.get(6)?,
        notes: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn map_receipt(row: &rusqlite::Row<'_>) -> rusqlite::Result<TravelReceipt> {
    Ok(TravelReceipt {
        id: row.get(0)?,
        account_id: row.get(1)?,
        folder: row.get(2)?,
        uidvalidity: row.get(3)?,
        uid: row.get(4)?,
        message_id: row.get(5)?,
        content_hash: row.get(6)?,
        from_addr: row.get(7)?,
        subject: row.get(8)?,
        received_at: row.get(9)?,
        authenticated_sender_domain: row.get(10)?,
        body_text: row.get(11)?,
        extracted: json_from_row(row, 12)?,
        trip_id: row.get(13)?,
        status: row.get(14)?,
        quarantine_reason: row.get(15)?,
        quarantined_at: row.get(16)?,
        created_at: row.get(17)?,
        updated_at: row.get(18)?,
    })
}

fn map_ingest_failure(row: &rusqlite::Row<'_>) -> rusqlite::Result<TravelIngestFailure> {
    Ok(TravelIngestFailure {
        source: TravelMessageSourceKey {
            account_id: row.get(0)?,
            folder: row.get(1)?,
            uidvalidity: row.get(2)?,
            uid: row.get(3)?,
        },
        stage: row.get(4)?,
        attempt_count: row.get(5)?,
        last_error: row.get(6)?,
        first_failed_at: row.get(7)?,
        last_failed_at: row.get(8)?,
        quarantined_receipt_id: row.get(9)?,
        quarantined_at: row.get(10)?,
    })
}

fn map_booking(row: &rusqlite::Row<'_>) -> rusqlite::Result<TravelBooking> {
    Ok(TravelBooking {
        id: row.get(0)?,
        trip_id: row.get(1)?,
        receipt_id: row.get(2)?,
        kind: row.get(3)?,
        provider: row.get(4)?,
        title: row.get(5)?,
        confirmation_code: row.get(6)?,
        status: row.get(7)?,
        starts_at: row.get(8)?,
        ends_at: row.get(9)?,
        location: row.get(10)?,
        details: json_from_row(row, 11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

fn map_segment(row: &rusqlite::Row<'_>) -> rusqlite::Result<TravelSegment> {
    Ok(TravelSegment {
        id: row.get(0)?,
        trip_id: row.get(1)?,
        booking_id: row.get(2)?,
        kind: row.get(3)?,
        sequence: row.get(4)?,
        origin: row.get(5)?,
        destination: row.get(6)?,
        carrier: row.get(7)?,
        service_number: row.get(8)?,
        departs_at: row.get(9)?,
        arrives_at: row.get(10)?,
        status: row.get(11)?,
        details: json_from_row(row, 12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

fn map_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<TravelTask> {
    Ok(TravelTask {
        id: row.get(0)?,
        trip_id: row.get(1)?,
        title: row.get(2)?,
        notes: row.get(3)?,
        due_at: row.get(4)?,
        completed_at: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn map_alert(row: &rusqlite::Row<'_>) -> rusqlite::Result<TravelAlert> {
    Ok(TravelAlert {
        id: row.get(0)?,
        trip_id: row.get(1)?,
        booking_id: row.get(2)?,
        segment_id: row.get(3)?,
        dedupe_key: row.get(4)?,
        kind: row.get(5)?,
        severity: row.get(6)?,
        title: row.get(7)?,
        body: row.get(8)?,
        scheduled_at: row.get(9)?,
        triggered_at: row.get(10)?,
        acknowledged_at: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

fn enqueue_travel_alert_on(
    conn: &Connection,
    input: &NewTravelAlert,
) -> Result<TravelAlertEnqueueResult> {
    let inserted = conn.execute(
        "INSERT INTO travel_alerts
            (id, trip_id, booking_id, segment_id, dedupe_key, kind, severity, title,
             body, scheduled_at, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)
         ON CONFLICT(dedupe_key) DO NOTHING",
        params![
            input.id,
            input.trip_id,
            input.booking_id,
            input.segment_id,
            input.dedupe_key,
            input.kind,
            input.severity,
            input.title,
            input.body,
            input.scheduled_at,
            input.now,
        ],
    )?;
    let sql = format!("SELECT {ALERT_COLUMNS} FROM travel_alerts WHERE dedupe_key = ?1");
    let alert = conn
        .query_row(&sql, params![input.dedupe_key], map_alert)
        .optional()?
        .ok_or_else(|| {
            StoreError::Config(format!(
                "travel alert not found after enqueue: {} ({})",
                input.id, input.dedupe_key
            ))
        })?;
    Ok(TravelAlertEnqueueResult {
        alert,
        inserted: inserted == 1,
    })
}

fn map_share_link(row: &rusqlite::Row<'_>) -> rusqlite::Result<TravelShareLink> {
    Ok(TravelShareLink {
        id: row.get(0)?,
        trip_id: row.get(1)?,
        token_prefix: row.get(2)?,
        label: row.get(3)?,
        can_edit_tasks: row.get(4)?,
        expires_at: row.get(5)?,
        revoked_at: row.get(6)?,
        last_accessed_at: row.get(7)?,
        created_at: row.get(8)?,
    })
}

fn json_from_row(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<Value>> {
    let raw: Option<String> = row.get(index)?;
    raw.map(|json| {
        serde_json::from_str(&json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    })
    .transpose()
}

fn json_to_text(value: Option<&Value>) -> Result<Option<String>> {
    value
        .map(serde_json::to_string)
        .transpose()
        .map_err(Into::into)
}

fn required<T>(value: Option<T>, kind: &str, id: &str) -> Result<T> {
    value.ok_or_else(|| StoreError::Config(format!("{kind} not found after insert: {id}")))
}

fn ingest_travel_receipt_tx(
    tx: &Transaction<'_>,
    input: &NewTravelReceipt,
) -> Result<TravelReceiptIngestResult> {
    let normalized_message_id = normalize_message_id(input.message_id.as_deref());
    let content_hash = receipt_content_hash(input);
    let quarantine_reason = input
        .quarantine_reason
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty());

    if let Some((mut receipt, deduped_by)) =
        find_receipt_duplicate(tx, input, normalized_message_id.as_deref(), &content_hash)?
    {
        // A safer replay may escalate a pending row into quarantine, or
        // refresh an existing quarantine reason. Processed and dismissed
        // receipts are terminal: a duplicate must never resurrect them.
        if let Some(reason) = quarantine_reason
            && matches!(receipt.status.as_str(), "pending" | "quarantined")
        {
            tx.execute(
                "UPDATE travel_receipts SET status = 'quarantined',
                        quarantine_reason = ?2,
                        quarantined_at = COALESCE(quarantined_at, ?3),
                        updated_at = ?3 WHERE id = ?1",
                params![receipt.id, reason, input.ingested_at],
            )?;
            receipt = get_receipt_tx(tx, &receipt.id)?.ok_or_else(|| {
                StoreError::Config(format!(
                    "travel receipt disappeared during quarantine: {}",
                    receipt.id
                ))
            })?;
        }
        return Ok(TravelReceiptIngestResult {
            receipt,
            inserted: false,
            deduped_by: Some(deduped_by),
        });
    }

    let extracted_json = json_to_text(input.extracted.as_ref())?;
    let (status, quarantined_at) = if quarantine_reason.is_some() {
        ("quarantined", Some(input.ingested_at.as_str()))
    } else {
        ("pending", None)
    };
    tx.execute(
        "INSERT INTO travel_receipts
            (id, account_id, folder, uidvalidity, uid, message_id,
             normalized_message_id, content_hash, from_addr, subject, received_at,
             authenticated_sender_domain, body_text, extracted_json, trip_id, status,
             quarantine_reason, quarantined_at, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 ?14, ?15, ?16, ?17, ?18, ?19, ?19)",
        params![
            input.id,
            input.account_id,
            input.folder,
            input.uidvalidity,
            input.uid,
            input.message_id,
            normalized_message_id,
            content_hash,
            input.from_addr,
            input.subject,
            input.received_at,
            input.authenticated_sender_domain,
            input.body_text,
            extracted_json,
            input.trip_id,
            status,
            quarantine_reason,
            quarantined_at,
            input.ingested_at,
        ],
    )?;
    let receipt = get_receipt_tx(tx, &input.id)?.ok_or_else(|| {
        StoreError::Config(format!(
            "travel receipt not found after ingest: {}",
            input.id
        ))
    })?;
    Ok(TravelReceiptIngestResult {
        receipt,
        inserted: true,
        deduped_by: None,
    })
}

fn get_receipt_tx(tx: &Transaction<'_>, id: &str) -> Result<Option<TravelReceipt>> {
    let sql = format!("SELECT {RECEIPT_COLUMNS} FROM travel_receipts WHERE id = ?1");
    Ok(tx.query_row(&sql, params![id], map_receipt).optional()?)
}

fn terminal_receipt_transition_tx(
    tx: &Transaction<'_>,
    id: &str,
    changed: usize,
) -> Result<TravelReceiptTerminalTransition> {
    let receipt = get_receipt_tx(tx, id)?;
    match (changed, receipt) {
        (1, Some(receipt)) => Ok(TravelReceiptTerminalTransition::Applied(receipt)),
        (0, Some(receipt)) => Ok(TravelReceiptTerminalTransition::Conflict(receipt)),
        (0, None) => Ok(TravelReceiptTerminalTransition::NotFound),
        (_, None) => Err(StoreError::Config(format!(
            "travel receipt disappeared during terminal transition: {id}"
        ))),
        (changed, Some(_)) => Err(StoreError::Config(format!(
            "terminal receipt transition changed {changed} rows for id {id}"
        ))),
    }
}

fn find_receipt_duplicate(
    tx: &Transaction<'_>,
    input: &NewTravelReceipt,
    normalized_message_id: Option<&str>,
    content_hash: &str,
) -> Result<Option<(TravelReceipt, ReceiptDedupeKey)>> {
    let source_sql = format!(
        "SELECT {RECEIPT_COLUMNS} FROM travel_receipts
         WHERE account_id = ?1 AND folder = ?2 AND uidvalidity = ?3 AND uid = ?4"
    );
    if let Some(receipt) = tx
        .query_row(
            &source_sql,
            params![input.account_id, input.folder, input.uidvalidity, input.uid],
            map_receipt,
        )
        .optional()?
    {
        return Ok(Some((receipt, ReceiptDedupeKey::MailboxUid)));
    }

    if let Some(message_id) = normalized_message_id {
        let message_sql = format!(
            "SELECT {RECEIPT_COLUMNS} FROM travel_receipts
             WHERE account_id = ?1 AND normalized_message_id = ?2"
        );
        if let Some(receipt) = tx
            .query_row(
                &message_sql,
                params![input.account_id, message_id],
                map_receipt,
            )
            .optional()?
        {
            return Ok(Some((receipt, ReceiptDedupeKey::MessageId)));
        }
    }

    let content_sql = format!(
        "SELECT {RECEIPT_COLUMNS} FROM travel_receipts
         WHERE account_id = ?1 AND content_hash = ?2"
    );
    Ok(tx
        .query_row(
            &content_sql,
            params![input.account_id, content_hash],
            map_receipt,
        )
        .optional()?
        .map(|receipt| (receipt, ReceiptDedupeKey::ContentHash)))
}

fn normalize_message_id(message_id: Option<&str>) -> Option<String> {
    message_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| {
            id.trim_start_matches('<')
                .trim_end_matches('>')
                .to_lowercase()
        })
        .filter(|id| !id.is_empty())
}

fn receipt_content_hash(input: &NewTravelReceipt) -> String {
    let mut digest = Sha256::new();
    for value in [
        input.from_addr.as_deref(),
        input.subject.as_deref(),
        input.received_at.as_deref(),
        input.body_text.as_deref(),
    ] {
        if let Some(value) = value {
            digest.update(value.trim().as_bytes());
        }
        digest.update([0]);
    }
    hex_digest(digest.finalize())
}

fn generate_share_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!("travel_{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// Derive a one-way, calendar-only capability from a page capability. Page
/// holders can subscribe, while a calendar provider cannot recover the page
/// token or inherit task-edit authority.
pub fn derive_travel_calendar_token(page_token: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"Envelope Travel calendar capability\0");
    digest.update(page_token.as_bytes());
    format!("calendar_{}", URL_SAFE_NO_PAD.encode(digest.finalize()))
}

fn hash_share_token(token: &str) -> String {
    hex_digest(Sha256::digest(token.as_bytes()))
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-08-31T12:00:00Z";

    fn trip_input() -> NewTrip {
        NewTrip {
            id: "trip-1".into(),
            title: "Paris weekend".into(),
            destination: Some("Paris".into()),
            starts_at: Some("2026-09-10T08:00:00Z".into()),
            ends_at: Some("2026-09-13T20:00:00Z".into()),
            timezone: "Europe/Paris".into(),
            status: "confirmed".into(),
            notes: Some("private trip note".into()),
            now: NOW.into(),
        }
    }

    fn receipt_input(id: &str, uid: i64, message_id: Option<&str>) -> NewTravelReceipt {
        NewTravelReceipt {
            id: id.into(),
            account_id: "gmail".into(),
            folder: "INBOX".into(),
            uidvalidity: 77,
            uid,
            message_id: message_id.map(str::to_string),
            from_addr: Some("tickets@example.test".into()),
            subject: Some("Your booking".into()),
            received_at: Some("2026-08-30T10:00:00Z".into()),
            authenticated_sender_domain: None,
            body_text: Some("Confirmation PRIVATE42".into()),
            extracted: Some(serde_json::json!({"kind": "flight"})),
            trip_id: Some("trip-1".into()),
            quarantine_reason: None,
            ingested_at: NOW.into(),
        }
    }

    fn gmail_settings(updated_at: &str) -> TravelSettings {
        TravelSettings {
            account_id: None,
            scan_folder: "INBOX".into(),
            home_timezone: "Europe/Paris".into(),
            calendar_name: "Family travel".into(),
            auto_ingest: true,
            default_alert_minutes: 1440,
            updated_at: updated_at.into(),
        }
    }

    fn write_atomic_itinerary(db: &Database, receipt_id: &str) -> Result<(Option<String>, String)> {
        let trip = db.create_trip(&NewTrip {
            id: "atomic-trip".into(),
            title: "Atomic Paris trip".into(),
            destination: Some("Paris".into()),
            starts_at: Some("2026-09-10T08:00:00Z".into()),
            ends_at: Some("2026-09-10T10:00:00Z".into()),
            timezone: "Europe/Paris".into(),
            status: "upcoming".into(),
            notes: None,
            now: NOW.into(),
        })?;
        let booking = db.create_travel_booking(&NewTravelBooking {
            id: "atomic-booking".into(),
            trip_id: trip.id.clone(),
            receipt_id: Some(receipt_id.into()),
            kind: "flight".into(),
            provider: Some("Air Example".into()),
            title: "JFK to CDG".into(),
            confirmation_code: Some("ATOMIC42".into()),
            status: "confirmed".into(),
            starts_at: Some("2026-09-10T08:00:00Z".into()),
            ends_at: Some("2026-09-10T10:00:00Z".into()),
            location: Some("CDG".into()),
            details: None,
            now: NOW.into(),
        })?;
        let segment = db.create_travel_segment(&NewTravelSegment {
            id: "atomic-segment".into(),
            trip_id: trip.id.clone(),
            booking_id: Some(booking.id.clone()),
            kind: "flight".into(),
            sequence: 0,
            origin: Some("JFK".into()),
            destination: Some("CDG".into()),
            carrier: Some("AE".into()),
            service_number: Some("42".into()),
            departs_at: Some("2026-09-10T08:00:00Z".into()),
            arrives_at: Some("2026-09-10T10:00:00Z".into()),
            status: "confirmed".into(),
            details: None,
            now: NOW.into(),
        })?;
        db.enqueue_travel_alert(&NewTravelAlert {
            id: "atomic-alert".into(),
            trip_id: Some(trip.id.clone()),
            booking_id: Some(booking.id),
            segment_id: Some(segment.id.clone()),
            dedupe_key: "atomic-booking:imported".into(),
            kind: "booking_imported".into(),
            severity: "info".into(),
            title: "Added: JFK to CDG".into(),
            body: None,
            scheduled_at: Some(NOW.into()),
            now: NOW.into(),
        })?;
        Ok((Some(trip.id), segment.id))
    }

    fn source_receipt(
        id: &str,
        account_id: &str,
        folder: &str,
        uidvalidity: i64,
        uid: i64,
        received_at: Option<&str>,
    ) -> TravelReceipt {
        TravelReceipt {
            id: id.into(),
            account_id: account_id.into(),
            folder: folder.into(),
            uidvalidity,
            uid,
            message_id: Some(format!("{id}@example.test")),
            content_hash: format!("hash-{id}"),
            from_addr: Some("tickets@example.test".into()),
            subject: Some("Travel update".into()),
            received_at: received_at.map(str::to_string),
            authenticated_sender_domain: Some("example.test".into()),
            body_text: None,
            extracted: None,
            trip_id: None,
            status: "processed".into(),
            quarantine_reason: None,
            quarantined_at: None,
            created_at: NOW.into(),
            updated_at: NOW.into(),
        }
    }

    #[test]
    fn receipt_source_order_uses_uid_only_inside_one_mailbox_epoch() {
        let current = source_receipt(
            "current",
            "gmail",
            "INBOX",
            77,
            100,
            Some("2026-09-02T10:00:00Z"),
        );
        let older_uid_with_later_header_date = source_receipt(
            "older",
            "gmail",
            "INBOX",
            77,
            20,
            Some("2026-09-03T10:00:00Z"),
        );
        let newer_uid_with_earlier_header_date = source_receipt(
            "newer",
            "gmail",
            "INBOX",
            77,
            101,
            Some("2026-09-01T10:00:00Z"),
        );

        assert_eq!(
            compare_travel_receipt_sources(&older_uid_with_later_header_date, &current),
            TravelReceiptSourceOrder::Older
        );
        assert_eq!(
            compare_travel_receipt_sources(&newer_uid_with_earlier_header_date, &current),
            TravelReceiptSourceOrder::Newer
        );
        assert_eq!(
            compare_travel_receipt_sources(&current, &current),
            TravelReceiptSourceOrder::Same
        );
    }

    #[test]
    fn receipt_source_order_uses_dates_across_epochs_and_fails_closed_without_them() {
        let current = source_receipt(
            "current",
            "gmail",
            "INBOX",
            77,
            100,
            Some("2026-09-02T10:00:00Z"),
        );
        let newer_epoch = source_receipt(
            "newer-epoch",
            "gmail",
            "INBOX",
            88,
            1,
            Some("2026-09-03T10:00:00Z"),
        );
        let unknown = source_receipt("unknown", "other", "Archive", 1, 999, None);
        let equal_time_other_source = source_receipt(
            "equal",
            "gmail",
            "Archive",
            9,
            1,
            Some("2026-09-02T10:00:00Z"),
        );

        assert_eq!(
            compare_travel_receipt_sources(&newer_epoch, &current),
            TravelReceiptSourceOrder::Newer
        );
        assert_eq!(
            compare_travel_receipt_sources(&unknown, &current),
            TravelReceiptSourceOrder::Incomparable
        );
        assert_eq!(
            compare_travel_receipt_sources(&equal_time_other_source, &current),
            TravelReceiptSourceOrder::Incomparable
        );
    }

    #[test]
    fn ingest_failures_retry_then_become_durable_skip_markers() {
        let db = Database::open_memory().unwrap();
        let source = TravelMessageSourceKey {
            account_id: "gmail".into(),
            folder: "INBOX".into(),
            uidvalidity: 77,
            uid: 42,
        };
        let first = db
            .record_travel_ingest_failure(&NewTravelIngestFailure {
                source: source.clone(),
                stage: "raw_fetch".into(),
                last_error: "temporary fetch failure".into(),
                now: NOW.into(),
            })
            .unwrap();
        assert_eq!(first.attempt_count, 1);
        let second = db
            .record_travel_ingest_failure(&NewTravelIngestFailure {
                source: source.clone(),
                stage: "decode".into(),
                last_error: "malformed message".into(),
                now: "2026-08-31T12:01:00Z".into(),
            })
            .unwrap();
        assert_eq!(second.attempt_count, 2);
        assert_eq!(second.stage, "decode");

        db.ingest_travel_receipt(&NewTravelReceipt {
            id: "poison-receipt".into(),
            account_id: source.account_id.clone(),
            folder: source.folder.clone(),
            uidvalidity: source.uidvalidity,
            uid: source.uid,
            message_id: None,
            from_addr: None,
            subject: Some("Unreadable travel email".into()),
            received_at: None,
            authenticated_sender_domain: None,
            body_text: Some("Envelope could not read this message.".into()),
            extracted: None,
            trip_id: None,
            quarantine_reason: Some("mailbox_message_unreadable_after_retries".into()),
            ingested_at: "2026-08-31T12:02:00Z".into(),
        })
        .unwrap();
        let quarantined = db
            .mark_travel_ingest_failure_quarantined(
                &source,
                "poison-receipt",
                "2026-08-31T12:02:00Z",
            )
            .unwrap()
            .unwrap();
        assert!(quarantined.quarantined_at.is_some());
        assert_eq!(
            db.list_quarantined_travel_ingest_sources("gmail", "INBOX", 77)
                .unwrap(),
            vec![source.clone()]
        );

        let replayed = db
            .record_travel_ingest_failure(&NewTravelIngestFailure {
                source: source.clone(),
                stage: "raw_fetch".into(),
                last_error: "should not replace durable state".into(),
                now: "2026-08-31T12:03:00Z".into(),
            })
            .unwrap();
        assert_eq!(replayed.attempt_count, 2);
        assert_eq!(replayed.stage, "decode");
        assert!(!db.clear_open_travel_ingest_failure(&source).unwrap());
    }

    #[test]
    fn settings_cursor_and_trip_crud_are_deterministic() {
        let db = Database::open_memory().unwrap();
        let settings = TravelSettings {
            account_id: Some("gmail".into()),
            scan_folder: "Receipts".into(),
            home_timezone: "Europe/Paris".into(),
            calendar_name: "Martin family travel".into(),
            auto_ingest: true,
            default_alert_minutes: 120,
            updated_at: NOW.into(),
        };
        assert_eq!(db.upsert_travel_settings(&settings).unwrap(), settings);

        let cursor = TravelIngestCursor {
            account_id: "gmail".into(),
            folder: "INBOX".into(),
            uidvalidity: 77,
            last_uid: 42,
            last_message_at: Some("2026-08-30T10:00:00Z".into()),
            updated_at: NOW.into(),
        };
        assert_eq!(db.upsert_travel_cursor(&cursor).unwrap(), cursor);
        let stale = TravelIngestCursor {
            last_uid: 10,
            ..cursor.clone()
        };
        assert_eq!(db.upsert_travel_cursor(&stale).unwrap().last_uid, 42);
        let next_epoch = TravelIngestCursor {
            uidvalidity: 78,
            last_uid: 2,
            ..cursor.clone()
        };
        assert_eq!(db.upsert_travel_cursor(&next_epoch).unwrap().last_uid, 2);
        assert_eq!(db.list_travel_cursors().unwrap(), vec![next_epoch]);

        let trip = db.create_trip(&trip_input()).unwrap();
        assert_eq!(trip.id, "trip-1");
        assert_eq!(trip.created_at, NOW);
        assert_eq!(db.list_trips().unwrap(), vec![trip.clone()]);

        let updated = db
            .update_trip(
                "trip-1",
                &TripUpdate {
                    title: "Paris and Reims".into(),
                    destination: Some("France".into()),
                    starts_at: trip.starts_at.clone(),
                    ends_at: trip.ends_at.clone(),
                    timezone: trip.timezone.clone(),
                    status: "in-progress".into(),
                    notes: trip.notes.clone(),
                    updated_at: "2026-09-10T09:00:00Z".into(),
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(updated.title, "Paris and Reims");
        assert_eq!(updated.created_at, NOW);
        assert!(db.delete_trip("trip-1").unwrap());
        assert!(db.get_trip("trip-1").unwrap().is_none());
    }

    #[test]
    fn gmail_account_and_travel_settings_upsert_atomically() {
        let db = Database::open_memory().unwrap();
        let passphrase = "test-passphrase";
        let mut requested_settings = gmail_settings(NOW);
        requested_settings.account_id = Some("caller-value-is-ignored".into());

        let (account, settings) = db
            .upsert_gmail_account_and_travel_settings(
                "Travel Gmail",
                "traveler@gmail.com",
                "fixture-app-password",
                passphrase,
                &requested_settings,
            )
            .unwrap();

        assert_eq!(account.username, "traveler@gmail.com");
        assert_eq!(account.imap_host, "imap.gmail.com");
        assert_eq!(account.smtp_host, "smtp.gmail.com");
        assert_eq!(settings.account_id.as_deref(), Some(account.id.as_str()));
        assert_eq!(db.get_travel_settings().unwrap(), Some(settings));
        assert_eq!(
            db.get_account_with_credentials(&account.id, passphrase)
                .unwrap()
                .effective_imap_password(),
            "fixture-app-password"
        );
    }

    #[test]
    fn gmail_travel_setup_rolls_back_new_account_when_settings_fail() {
        let db = Database::open_memory().unwrap();
        db.conn()
            .execute_batch(
                "CREATE TRIGGER reject_travel_settings_insert
                 BEFORE INSERT ON travel_settings
                 BEGIN
                   SELECT RAISE(ABORT, 'forced travel settings failure');
                 END;",
            )
            .unwrap();

        let result = db.upsert_gmail_account_and_travel_settings(
            "Travel Gmail",
            "new@gmail.com",
            "fixture-app-password",
            "test-passphrase",
            &gmail_settings(NOW),
        );

        assert!(result.is_err());
        assert!(db.find_account_by_email("new@gmail.com").unwrap().is_none());
        assert!(db.get_travel_settings().unwrap().is_none());
    }

    #[test]
    fn gmail_travel_setup_rolls_back_existing_credentials_when_settings_fail() {
        let db = Database::open_memory().unwrap();
        let passphrase = "test-passphrase";
        let (original, original_settings) = db
            .upsert_gmail_account_and_travel_settings(
                "Original Gmail",
                "existing@gmail.com",
                "original-app-password",
                passphrase,
                &gmail_settings(NOW),
            )
            .unwrap();
        db.conn()
            .execute_batch(
                "CREATE TRIGGER reject_travel_settings_insert
                 BEFORE INSERT ON travel_settings
                 BEGIN
                   SELECT RAISE(ABORT, 'forced travel settings failure');
                 END;
                 CREATE TRIGGER reject_travel_settings_update
                 BEFORE UPDATE ON travel_settings
                 BEGIN
                   SELECT RAISE(ABORT, 'forced travel settings failure');
                 END;",
            )
            .unwrap();
        let mut changed_settings = gmail_settings("2026-08-31T12:05:00Z");
        changed_settings.calendar_name = "Changed calendar".into();

        let result = db.upsert_gmail_account_and_travel_settings(
            "Changed Gmail",
            "existing@gmail.com",
            "changed-app-password",
            passphrase,
            &changed_settings,
        );

        assert!(result.is_err());
        let persisted = db
            .find_account_by_email("existing@gmail.com")
            .unwrap()
            .unwrap();
        assert_eq!(persisted.id, original.id);
        assert_eq!(persisted.name, "Original Gmail");
        assert_eq!(
            db.get_account_with_credentials(&persisted.id, passphrase)
                .unwrap()
                .effective_imap_password(),
            "original-app-password"
        );
        assert_eq!(db.get_travel_settings().unwrap(), Some(original_settings));
    }

    #[test]
    fn receipt_ingest_dedupes_by_source_message_id_and_content() {
        let db = Database::open_memory().unwrap();
        db.create_trip(&trip_input()).unwrap();

        let first = db
            .ingest_travel_receipt(&receipt_input("receipt-1", 1, Some("<ABC@EXAMPLE>")))
            .unwrap();
        assert!(first.inserted);

        let same_source = db
            .ingest_travel_receipt(&receipt_input("receipt-2", 1, Some("different@example")))
            .unwrap();
        assert!(!same_source.inserted);
        assert_eq!(same_source.receipt.id, "receipt-1");
        assert_eq!(same_source.deduped_by, Some(ReceiptDedupeKey::MailboxUid));

        let mut same_message = receipt_input("receipt-3", 3, Some("abc@example"));
        same_message.body_text = Some("different body".into());
        let same_message = db.ingest_travel_receipt(&same_message).unwrap();
        assert_eq!(same_message.deduped_by, Some(ReceiptDedupeKey::MessageId));

        let same_content = db
            .ingest_travel_receipt(&receipt_input("receipt-4", 4, Some("new@example")))
            .unwrap();
        assert_eq!(same_content.deduped_by, Some(ReceiptDedupeKey::ContentHash));
        assert_eq!(db.list_travel_receipts(None, true, 100).unwrap().len(), 1);

        let mut safer_replay = receipt_input("receipt-5", 5, Some("safer@example"));
        safer_replay.quarantine_reason = Some("parser confidence fell below threshold".into());
        let quarantined = db.ingest_travel_receipt(&safer_replay).unwrap();
        assert!(!quarantined.inserted);
        assert_eq!(quarantined.receipt.status, "quarantined");
        assert_eq!(db.list_quarantined_travel_receipts(10).unwrap().len(), 1);
    }

    #[test]
    fn quarantined_duplicate_does_not_resurrect_terminal_receipt() {
        let db = Database::open_memory().unwrap();
        db.create_trip(&trip_input()).unwrap();

        let processed = db
            .ingest_travel_receipt(&receipt_input("processed", 1, Some("processed@example")))
            .unwrap()
            .receipt;
        db.mark_travel_receipt_processed(
            &processed.id,
            Some("trip-1"),
            Some(&serde_json::json!({"kind": "flight", "final": true})),
            "2026-08-31T12:01:00Z",
        )
        .unwrap();

        let mut processed_replay = receipt_input("processed-replay", 1, Some("different@example"));
        processed_replay.quarantine_reason = Some("parser confidence fell".into());
        processed_replay.ingested_at = "2026-08-31T12:02:00Z".into();
        let replayed = db.ingest_travel_receipt(&processed_replay).unwrap();
        assert!(!replayed.inserted);
        assert_eq!(replayed.deduped_by, Some(ReceiptDedupeKey::MailboxUid));
        assert_eq!(replayed.receipt.status, "processed");
        assert_eq!(replayed.receipt.trip_id.as_deref(), Some("trip-1"));
        assert_eq!(
            replayed.receipt.extracted,
            Some(serde_json::json!({"kind": "flight", "final": true}))
        );
        assert!(replayed.receipt.quarantine_reason.is_none());

        let mut dismissed_input = receipt_input("dismissed", 2, Some("dismissed@example"));
        dismissed_input.body_text = Some("A different receipt".into());
        let dismissed = db.ingest_travel_receipt(&dismissed_input).unwrap().receipt;
        db.dismiss_travel_receipt(&dismissed.id, "2026-08-31T12:03:00Z")
            .unwrap();

        let mut dismissed_replay =
            receipt_input("dismissed-replay", 20, Some("<DISMISSED@EXAMPLE>"));
        dismissed_replay.body_text = Some("Another changed body".into());
        dismissed_replay.quarantine_reason = Some("ambiguous travel date".into());
        dismissed_replay.ingested_at = "2026-08-31T12:04:00Z".into();
        let replayed = db.ingest_travel_receipt(&dismissed_replay).unwrap();
        assert!(!replayed.inserted);
        assert_eq!(replayed.deduped_by, Some(ReceiptDedupeKey::MessageId));
        assert_eq!(replayed.receipt.status, "dismissed");
        assert!(replayed.receipt.quarantine_reason.is_none());
        assert!(db.list_quarantined_travel_receipts(10).unwrap().is_empty());
    }

    #[test]
    fn terminal_receipt_transitions_are_compare_and_swap() {
        let db = Database::open_memory().unwrap();
        db.create_trip(&trip_input()).unwrap();

        let processed = db
            .ingest_travel_receipt(&receipt_input("processed", 1, Some("processed@example")))
            .unwrap()
            .receipt;
        let applied = db
            .mark_travel_receipt_processed(
                &processed.id,
                Some("trip-1"),
                Some(&serde_json::json!({"kind": "flight"})),
                "2026-08-31T12:01:00Z",
            )
            .unwrap();
        assert!(matches!(
            applied,
            TravelReceiptTerminalTransition::Applied(ref receipt)
                if receipt.status == "processed"
        ));
        let conflict = db
            .dismiss_travel_receipt(&processed.id, "2026-08-31T12:02:00Z")
            .unwrap();
        assert!(matches!(
            conflict,
            TravelReceiptTerminalTransition::Conflict(ref receipt)
                if receipt.status == "processed"
        ));
        assert!(
            db.quarantine_travel_receipt(&processed.id, "late parser replay", NOW)
                .unwrap()
                .is_none()
        );
        assert!(
            db.release_travel_receipt(&processed.id, NOW)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.get_travel_receipt(&processed.id)
                .unwrap()
                .unwrap()
                .status,
            "processed"
        );

        let mut dismissed_input = receipt_input("dismissed", 2, Some("dismissed@example"));
        dismissed_input.body_text = Some("Different content".into());
        let dismissed = db.ingest_travel_receipt(&dismissed_input).unwrap().receipt;
        assert!(matches!(
            db.dismiss_travel_receipt(&dismissed.id, "2026-08-31T12:03:00Z")
                .unwrap(),
            TravelReceiptTerminalTransition::Applied(ref receipt)
                if receipt.status == "dismissed"
        ));
        assert!(matches!(
            db.mark_travel_receipt_processed(
                &dismissed.id,
                Some("trip-1"),
                Some(&serde_json::json!({"kind": "flight"})),
                "2026-08-31T12:04:00Z",
            )
            .unwrap(),
            TravelReceiptTerminalTransition::Conflict(ref receipt)
                if receipt.status == "dismissed"
        ));
        assert!(matches!(
            db.dismiss_travel_receipt("missing", NOW).unwrap(),
            TravelReceiptTerminalTransition::NotFound
        ));
    }

    #[test]
    fn atomic_receipt_processing_rolls_back_terminal_failure_and_replays_cleanly() {
        let db = Database::open_memory().unwrap();
        let mut input = receipt_input("atomic-receipt", 41, Some("atomic@example"));
        input.trip_id = None;
        let receipt = db.ingest_travel_receipt(&input).unwrap().receipt;
        db.conn()
            .execute_batch(
                "CREATE TRIGGER reject_atomic_receipt_terminal_update
                 BEFORE UPDATE OF status ON travel_receipts
                 WHEN OLD.id = 'atomic-receipt' AND NEW.status = 'processed'
                 BEGIN
                   SELECT RAISE(ABORT, 'forced terminal transition failure');
                 END;",
            )
            .unwrap();

        let failed = db.process_travel_receipt_atomically(
            &receipt.id,
            Some(&serde_json::json!({"kind": "flight", "atomic": true})),
            "2026-08-31T12:01:00Z",
            |db| write_atomic_itinerary(db, &receipt.id),
        );
        assert!(failed.is_err());
        assert_eq!(
            db.get_travel_receipt(&receipt.id).unwrap().unwrap().status,
            "pending"
        );
        assert!(db.list_trips().unwrap().is_empty());
        assert!(db.list_travel_alerts(None).unwrap().is_empty());
        for table in ["travel_bookings", "travel_segments"] {
            let count = db
                .conn()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table} escaped the failed transaction");
        }

        db.conn()
            .execute_batch("DROP TRIGGER reject_atomic_receipt_terminal_update;")
            .unwrap();
        let replayed = db
            .process_travel_receipt_atomically(
                &receipt.id,
                Some(&serde_json::json!({"kind": "flight", "atomic": true})),
                "2026-08-31T12:02:00Z",
                |db| write_atomic_itinerary(db, &receipt.id),
            )
            .unwrap();
        assert!(matches!(
            replayed,
            TravelReceiptAtomicTransition::Applied {
                ref receipt,
                ref value,
            } if receipt.status == "processed" && value == "atomic-segment"
        ));
        assert_eq!(db.list_trips().unwrap().len(), 1);
        assert_eq!(db.list_travel_bookings("atomic-trip").unwrap().len(), 1);
        assert_eq!(db.list_travel_segments("atomic-trip").unwrap().len(), 1);
        assert_eq!(db.list_travel_alerts(Some("atomic-trip")).unwrap().len(), 1);
    }

    #[test]
    fn review_alert_transitions_are_atomic_and_respect_terminal_cas() {
        let db = Database::open_memory().unwrap();
        let mut input = receipt_input("review-receipt", 42, Some("review@example"));
        input.trip_id = None;
        let receipt = db.ingest_travel_receipt(&input).unwrap().receipt;
        let alert = NewTravelAlert {
            id: "review-alert".into(),
            trip_id: None,
            booking_id: None,
            segment_id: None,
            dedupe_key: format!("receipt:{}:review", receipt.id),
            kind: "receipt_review".into(),
            severity: "attention".into(),
            title: "Review travel receipt".into(),
            body: Some("The parser needs owner review.".into()),
            scheduled_at: None,
            now: NOW.into(),
        };

        db.conn()
            .execute_batch(
                "CREATE TRIGGER reject_review_alert_insert
                 BEFORE INSERT ON travel_alerts
                 WHEN NEW.kind = 'receipt_review'
                 BEGIN
                   SELECT RAISE(ABORT, 'forced review alert failure');
                 END;",
            )
            .unwrap();
        assert!(
            db.quarantine_travel_receipt_with_review_alert(
                &receipt.id,
                "parser confidence fell below threshold",
                &alert,
                "2026-08-31T12:01:00Z",
            )
            .is_err()
        );
        assert_eq!(
            db.get_travel_receipt(&receipt.id).unwrap().unwrap().status,
            "pending"
        );
        assert!(db.list_travel_alerts(None).unwrap().is_empty());

        db.conn()
            .execute_batch("DROP TRIGGER reject_review_alert_insert;")
            .unwrap();
        let quarantined = db
            .quarantine_travel_receipt_with_review_alert(
                &receipt.id,
                "parser confidence fell below threshold",
                &alert,
                "2026-08-31T12:02:00Z",
            )
            .unwrap();
        assert!(matches!(
            quarantined,
            TravelReceiptReviewTransition::Applied(ref receipt)
                if receipt.status == "quarantined"
        ));
        let alerts = db.list_travel_alerts(None).unwrap();
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].acknowledged_at.is_none());

        db.conn()
            .execute_batch(
                "CREATE TRIGGER reject_review_alert_acknowledgement
                 BEFORE UPDATE OF acknowledged_at ON travel_alerts
                 WHEN NEW.acknowledged_at IS NOT NULL
                 BEGIN
                   SELECT RAISE(ABORT, 'forced review acknowledgement failure');
                 END;",
            )
            .unwrap();
        assert!(
            db.dismiss_travel_receipt(&receipt.id, "2026-08-31T12:03:00Z")
                .is_err()
        );
        assert_eq!(
            db.get_travel_receipt(&receipt.id).unwrap().unwrap().status,
            "quarantined"
        );
        assert!(
            db.list_travel_alerts(None).unwrap()[0]
                .acknowledged_at
                .is_none()
        );

        db.conn()
            .execute_batch("DROP TRIGGER reject_review_alert_acknowledgement;")
            .unwrap();
        assert!(matches!(
            db.dismiss_travel_receipt(&receipt.id, "2026-08-31T12:04:00Z")
                .unwrap(),
            TravelReceiptTerminalTransition::Applied(ref receipt)
                if receipt.status == "dismissed"
        ));
        assert!(
            db.list_travel_alerts(None).unwrap()[0]
                .acknowledged_at
                .is_some()
        );

        assert!(matches!(
            db.quarantine_travel_receipt_with_review_alert(
                &receipt.id,
                "late parser replay",
                &alert,
                "2026-08-31T12:05:00Z",
            )
            .unwrap(),
            TravelReceiptReviewTransition::Conflict(ref receipt)
                if receipt.status == "dismissed"
        ));
        assert_eq!(db.list_travel_alerts(None).unwrap().len(), 1);
    }

    #[test]
    fn concurrent_terminal_receipt_transitions_have_one_winner() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("travel-race.db");
        {
            let db = Database::open(&path).unwrap();
            db.create_trip(&trip_input()).unwrap();
            db.ingest_travel_receipt(&receipt_input("race", 1, Some("race@example")))
                .unwrap();
        }
        let processing_db = Database::open(&path).unwrap();
        let dismissing_db = Database::open(&path).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let processing_barrier = barrier.clone();
        let processing = std::thread::spawn(move || {
            processing_barrier.wait();
            processing_db
                .mark_travel_receipt_processed(
                    "race",
                    Some("trip-1"),
                    Some(&serde_json::json!({"kind": "flight"})),
                    "2026-08-31T12:01:00Z",
                )
                .unwrap()
        });
        let dismissing_barrier = barrier.clone();
        let dismissing = std::thread::spawn(move || {
            dismissing_barrier.wait();
            dismissing_db
                .dismiss_travel_receipt("race", "2026-08-31T12:02:00Z")
                .unwrap()
        });
        barrier.wait();
        let transitions = [processing.join().unwrap(), dismissing.join().unwrap()];

        let applied = transitions
            .iter()
            .filter_map(|transition| match transition {
                TravelReceiptTerminalTransition::Applied(receipt) => Some(receipt),
                _ => None,
            })
            .collect::<Vec<_>>();
        let conflicts = transitions
            .iter()
            .filter_map(|transition| match transition {
                TravelReceiptTerminalTransition::Conflict(receipt) => Some(receipt),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(applied.len(), 1);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(applied[0].status, conflicts[0].status);
        assert!(matches!(
            applied[0].status.as_str(),
            "processed" | "dismissed"
        ));
        assert_eq!(
            Database::open(&path)
                .unwrap()
                .get_travel_receipt("race")
                .unwrap()
                .unwrap()
                .status,
            applied[0].status
        );
    }

    #[test]
    fn booking_segment_and_quarantine_crud() {
        let db = Database::open_memory().unwrap();
        db.create_trip(&trip_input()).unwrap();
        let receipt = db
            .ingest_travel_receipt(&receipt_input("receipt-1", 1, Some("r1@example")))
            .unwrap()
            .receipt;
        let quarantined = db
            .quarantine_travel_receipt(&receipt.id, "parser confidence too low", NOW)
            .unwrap()
            .unwrap();
        assert_eq!(quarantined.status, "quarantined");
        assert_eq!(db.list_travel_receipts(None, false, 100).unwrap().len(), 0);
        assert_eq!(
            db.release_travel_receipt(&receipt.id, NOW)
                .unwrap()
                .unwrap()
                .status,
            "pending"
        );

        let booking = db
            .create_travel_booking(&NewTravelBooking {
                id: "booking-1".into(),
                trip_id: "trip-1".into(),
                receipt_id: Some(receipt.id.clone()),
                kind: "flight".into(),
                provider: Some("Air Example".into()),
                title: "Outbound".into(),
                confirmation_code: Some("PRIVATE42".into()),
                status: "confirmed".into(),
                starts_at: Some("2026-09-10T08:00:00Z".into()),
                ends_at: Some("2026-09-10T10:00:00Z".into()),
                location: Some("CDG".into()),
                details: Some(serde_json::json!({"seat":"1A"})),
                now: NOW.into(),
            })
            .unwrap();
        db.create_travel_segment(&NewTravelSegment {
            id: "segment-1".into(),
            trip_id: "trip-1".into(),
            booking_id: Some(booking.id.clone()),
            kind: "flight".into(),
            sequence: 1,
            origin: Some("JFK".into()),
            destination: Some("CDG".into()),
            carrier: Some("EX".into()),
            service_number: Some("100".into()),
            departs_at: Some("2026-09-10T08:00:00Z".into()),
            arrives_at: Some("2026-09-10T10:00:00Z".into()),
            status: "scheduled".into(),
            details: None,
            now: NOW.into(),
        })
        .unwrap();
        assert_eq!(db.list_travel_bookings("trip-1").unwrap().len(), 1);
        assert_eq!(db.list_travel_segments("trip-1").unwrap().len(), 1);
        assert!(db.delete_travel_booking(&booking.id).unwrap());
        assert!(db.list_travel_segments("trip-1").unwrap().is_empty());
        assert!(db.delete_travel_receipt(&receipt.id).unwrap());
    }

    #[test]
    fn task_toggle_round_trips() {
        let db = Database::open_memory().unwrap();
        db.create_trip(&trip_input()).unwrap();
        db.create_travel_task(&NewTravelTask {
            id: "task-1".into(),
            trip_id: Some("trip-1".into()),
            title: "Check in".into(),
            notes: Some("private task note".into()),
            due_at: Some("2026-09-09T08:00:00Z".into()),
            now: NOW.into(),
        })
        .unwrap();
        assert_eq!(db.list_travel_tasks(None).unwrap().len(), 1);

        let completed = db
            .toggle_travel_task("task-1", "2026-09-09T07:00:00Z")
            .unwrap()
            .unwrap();
        assert_eq!(
            completed.completed_at.as_deref(),
            Some("2026-09-09T07:00:00Z")
        );
        let reopened = db
            .toggle_travel_task("task-1", "2026-09-09T07:01:00Z")
            .unwrap()
            .unwrap();
        assert!(reopened.completed_at.is_none());
    }

    #[test]
    fn alerts_are_idempotent_on_semantic_dedupe_key() {
        let db = Database::open_memory().unwrap();
        db.create_trip(&trip_input()).unwrap();
        let alert = NewTravelAlert {
            id: "alert-1".into(),
            trip_id: Some("trip-1".into()),
            booking_id: None,
            segment_id: None,
            dedupe_key: "trip-1:check-in:24h".into(),
            kind: "check-in".into(),
            severity: "info".into(),
            title: "Check in now".into(),
            body: Some("Airline check-in is open".into()),
            scheduled_at: Some("2026-09-09T08:00:00Z".into()),
            now: NOW.into(),
        };
        assert!(db.enqueue_travel_alert(&alert).unwrap().inserted);
        let replay = NewTravelAlert {
            id: "alert-2".into(),
            ..alert
        };
        let replayed = db.enqueue_travel_alert(&replay).unwrap();
        assert!(!replayed.inserted);
        assert_eq!(replayed.alert.id, "alert-1");
        assert_eq!(db.list_travel_alerts(Some("trip-1")).unwrap().len(), 1);
        assert_eq!(db.list_travel_alerts(None).unwrap().len(), 1);
    }

    #[test]
    fn reminder_reconciliation_replaces_stale_rows_and_preserves_other_alerts() {
        let db = Database::open_memory().unwrap();
        db.create_trip(&trip_input()).unwrap();
        db.create_travel_booking(&NewTravelBooking {
            id: "booking-1".into(),
            trip_id: "trip-1".into(),
            receipt_id: None,
            kind: "flight".into(),
            provider: Some("Air Example".into()),
            title: "Outbound".into(),
            confirmation_code: None,
            status: "confirmed".into(),
            starts_at: Some("2026-09-10T08:00:00Z".into()),
            ends_at: Some("2026-09-10T10:00:00Z".into()),
            location: Some("CDG".into()),
            details: None,
            now: NOW.into(),
        })
        .unwrap();
        for (id, dedupe_key, kind) in [
            ("old", "booking:booking-1:reminder:check_in:old", "check_in"),
            (
                "current",
                "booking:booking-1:reminder:check_in:current",
                "check_in",
            ),
            (
                "changed",
                "booking:booking-1:booking_changed:now",
                "booking_changed",
            ),
        ] {
            db.enqueue_travel_alert(&NewTravelAlert {
                id: id.into(),
                trip_id: Some("trip-1".into()),
                booking_id: Some("booking-1".into()),
                segment_id: None,
                dedupe_key: dedupe_key.into(),
                kind: kind.into(),
                severity: "attention".into(),
                title: id.into(),
                body: None,
                scheduled_at: Some("2026-09-09T08:00:00Z".into()),
                now: NOW.into(),
            })
            .unwrap();
        }

        assert_eq!(
            db.delete_travel_booking_reminders_except(
                "booking-1",
                Some("booking:booking-1:reminder:check_in:current")
            )
            .unwrap(),
            1
        );
        let remaining = db.list_travel_alerts(Some("trip-1")).unwrap();
        assert!(remaining.iter().any(|alert| alert.id == "current"));
        assert!(remaining.iter().any(|alert| alert.id == "changed"));
        assert!(!remaining.iter().any(|alert| alert.id == "old"));

        assert_eq!(
            db.delete_travel_booking_reminders_except("booking-1", None)
                .unwrap(),
            1
        );
        let remaining = db.list_travel_alerts(Some("trip-1")).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "changed");
    }

    #[test]
    fn share_tokens_are_hash_only_revocable_and_public_safe() {
        let db = Database::open_memory().unwrap();
        db.create_trip(&trip_input()).unwrap();
        db.create_travel_booking(&NewTravelBooking {
            id: "booking-1".into(),
            trip_id: "trip-1".into(),
            receipt_id: None,
            kind: "hotel".into(),
            provider: Some("Example Hotel".into()),
            title: "Three nights".into(),
            confirmation_code: Some("DO-NOT-SHARE".into()),
            status: "confirmed".into(),
            starts_at: Some("2026-09-10T15:00:00Z".into()),
            ends_at: Some("2026-09-13T10:00:00Z".into()),
            location: Some("Paris".into()),
            details: Some(serde_json::json!({"private":"also hidden"})),
            now: NOW.into(),
        })
        .unwrap();
        let created = db
            .create_travel_share_link(&NewTravelShareLink {
                id: "share-1".into(),
                trip_id: "trip-1".into(),
                label: Some("Family".into()),
                can_edit_tasks: true,
                expires_at: Some("2026-12-31T00:00:00Z".into()),
                created_at: NOW.into(),
            })
            .unwrap();

        let stored: (String, String, String) = db
            .conn()
            .query_row(
                "SELECT token_hash, calendar_token_hash, token_prefix
                 FROM travel_share_links WHERE id = 'share-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_ne!(stored.0, created.token);
        assert!(!stored.0.contains(&created.token));
        assert_ne!(stored.1, created.calendar_token);
        assert!(!stored.1.contains(&created.calendar_token));
        assert!(created.token.starts_with(&stored.2));
        assert!(
            db.lookup_travel_share_link(&created.calendar_token, "2026-09-01T00:00:00Z")
                .unwrap()
                .is_none()
        );
        assert!(
            db.lookup_travel_calendar_share_link(&created.token, "2026-09-01T00:00:00Z")
                .unwrap()
                .is_none()
        );

        let view = db
            .public_trip_view(&created.token, "2026-09-01T00:00:00Z")
            .unwrap()
            .unwrap();
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("DO-NOT-SHARE"));
        assert!(!json.contains("private trip note"));
        assert!(!json.contains("also hidden"));
        assert!(view.can_edit_tasks);
        assert!(
            db.public_trip_calendar_view(&created.calendar_token, "2026-09-01T00:00:00Z")
                .unwrap()
                .is_some()
        );

        db.revoke_travel_share_link("share-1", "2026-09-02T00:00:00Z")
            .unwrap();
        assert!(
            db.lookup_travel_share_link(&created.token, "2026-09-03T00:00:00Z")
                .unwrap()
                .is_none()
        );
        assert!(
            db.lookup_travel_calendar_share_link(&created.calendar_token, "2026-09-03T00:00:00Z")
                .unwrap()
                .is_none()
        );
    }
}
