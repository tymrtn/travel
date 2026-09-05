//! Owner operations and projections for local calendar clients.
use crate::{
    state::AppState,
    travel_ics::{TravelCalendarEvent, render_travel_calendar, valid_action_url},
};
use anyhow::{Result, ensure};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use envelope_email_store::Database;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub fn initialize(db: &Database) -> Result<()> {
    db.conn().execute_batch("CREATE TABLE IF NOT EXISTS travel_action_links(
      id TEXT PRIMARY KEY, booking_id TEXT NOT NULL REFERENCES travel_bookings(id),
      purpose TEXT NOT NULL, label TEXT NOT NULL, url TEXT NOT NULL,
      visibility TEXT NOT NULL DEFAULT 'private', provenance TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS travel_travelers(id TEXT PRIMARY KEY,name TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS travel_booking_travelers(booking_id TEXT NOT NULL,traveler_id TEXT NOT NULL,
      PRIMARY KEY(booking_id,traveler_id));")?;
    Ok(())
}
fn response(result: Result<Value>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":error.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Serialize, Deserialize)]
pub struct ActionLink {
    pub purpose: String,
    pub label: String,
    pub url: String,
    #[serde(default = "private")]
    pub visibility: String,
}
fn private() -> String {
    "private".into()
}
pub async fn link(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(link): Json<ActionLink>,
) -> Response {
    let db = state.db.lock().await;
    response((|| {
        initialize(&db)?;
        ensure!(db.get_travel_booking(&id)?.is_some(), "Booking not found");
        ensure!(valid_action_url(&link.url), "Invalid action URL");
        ensure!(
            [
                "manage",
                "check_in",
                "flight_status",
                "directions",
                "ticket",
                "cancel"
            ]
            .contains(&link.purpose.as_str()),
            "Invalid purpose"
        );
        ensure!(
            link.visibility == "private" || link.visibility == "shared",
            "Invalid visibility"
        );
        if link.visibility == "shared" {
            let parsed = url::Url::parse(&link.url)?;
            ensure!(
                ["flight_status", "directions"].contains(&link.purpose.as_str())
                    && parsed.query().is_none()
                    && parsed.fragment().is_none(),
                "Shared links must be informational and contain no query or fragment"
            );
        }
        let key = uuid::Uuid::new_v4().to_string();
        let tx = db.conn().unchecked_transaction()?;
        db.conn().execute(
            "INSERT INTO travel_action_links VALUES(?1,?2,?3,?4,?5,?6,'owner')",
            rusqlite::params![key, id, link.purpose, link.label, link.url, link.visibility],
        )?;
        let now = chrono::Utc::now().to_rfc3339();
        db.conn().execute(
            "UPDATE travel_bookings SET updated_at=?2 WHERE id=?1",
            rusqlite::params![id, now],
        )?;
        db.conn().execute(
            "UPDATE travel_segments SET updated_at=?2 WHERE booking_id=?1",
            rusqlite::params![id, now],
        )?;
        tx.commit()?;
        Ok(json!({"id":key}))
    })())
}

#[derive(Deserialize)]
pub struct BookingEdit {
    pub expected_updated_at: String,
    pub title: String,
    pub status: String,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub location: Option<String>,
}
fn valid_dates(start: Option<&str>, end: Option<&str>) -> Result<()> {
    fn parse(value: &str) -> Result<chrono::DateTime<chrono::Utc>> {
        if let Ok(date) = chrono::DateTime::parse_from_rfc3339(value) {
            return Ok(date.with_timezone(&chrono::Utc));
        }
        Ok(chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")?
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc())
    }
    let start = start.map(parse).transpose()?;
    let end = end.map(parse).transpose()?;
    ensure!(
        start.zip(end).is_none_or(|(start, end)| end >= start),
        "End precedes start"
    );
    Ok(())
}
pub async fn edit_booking(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<BookingEdit>,
) -> Response {
    let db = state.db.lock().await;
    response((|| {
        ensure!(
            !input.title.trim().is_empty() && input.title.len() <= 500,
            "Title required (maximum 500 characters)"
        );
        ensure!(
            ["confirmed", "changed", "cancelled", "pending"].contains(&input.status.as_str()),
            "Invalid booking status"
        );
        valid_dates(input.starts_at.as_deref(), input.ends_at.as_deref())?;
        let tx = db.conn().unchecked_transaction()?;
        let previous = db
            .get_travel_booking(&id)?
            .ok_or_else(|| anyhow::anyhow!("Booking not found"))?;
        ensure!(
            previous.updated_at == input.expected_updated_at,
            "Booking changed; refresh before editing"
        );
        let segments = db
            .list_travel_segments(&previous.trip_id)?
            .into_iter()
            .filter(|s| s.booking_id.as_deref() == Some(&id))
            .collect::<Vec<_>>();
        let dates_changed =
            previous.starts_at != input.starts_at || previous.ends_at != input.ends_at;
        ensure!(
            !dates_changed || segments.len() <= 1,
            "Edit individual flight legs before changing a multi-leg itinerary"
        );
        let now = chrono::Utc::now().to_rfc3339();
        let booking = db
            .update_travel_booking(
                &id,
                &envelope_email_store::TravelBookingUpdate {
                    receipt_id: previous.receipt_id,
                    kind: previous.kind,
                    provider: previous.provider,
                    title: input.title,
                    confirmation_code: previous.confirmation_code,
                    status: input.status.clone(),
                    starts_at: input.starts_at.clone(),
                    ends_at: input.ends_at.clone(),
                    location: input.location,
                    details: previous.details,
                    updated_at: now.clone(),
                },
            )?
            .unwrap();
        if segments.len() == 1 && dates_changed {
            db.conn().execute("UPDATE travel_segments SET departs_at=?2,arrives_at=?3,updated_at=?4 WHERE booking_id=?1",rusqlite::params![id,input.starts_at,input.ends_at,now])?;
        }
        db.conn().execute(
            "UPDATE travel_segments SET status=?2,updated_at=?3 WHERE booking_id=?1",
            rusqlite::params![id, input.status, now],
        )?;
        let minutes = db
            .get_travel_settings()?
            .map(|s| s.default_alert_minutes)
            .unwrap_or(120);
        crate::handlers::travel::enqueue_booking_alerts_on(
            &db,
            &booking.trip_id,
            &booking,
            None,
            true,
            minutes,
            &now,
        )
        .map_err(anyhow::Error::msg)?;
        tx.commit()?;
        Ok(json!({"booking":booking}))
    })())
}

#[derive(Deserialize)]
pub struct TripEdit {
    pub expected_updated_at: String,
    pub title: String,
    pub destination: Option<String>,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub timezone: String,
    pub status: String,
    pub notes: Option<String>,
}
pub async fn edit_trip(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<TripEdit>,
) -> Response {
    let db = state.db.lock().await;
    response((|| {
        ensure!(
            !input.title.trim().is_empty() && input.title.len() <= 500,
            "Title required"
        );
        ensure!(
            ["planning", "upcoming", "active", "completed", "cancelled"]
                .contains(&input.status.as_str()),
            "Invalid trip status"
        );
        ensure!(
            !input.timezone.trim().is_empty() && !input.timezone.chars().any(char::is_control),
            "Invalid timezone"
        );
        valid_dates(input.starts_at.as_deref(), input.ends_at.as_deref())?;
        let tx = db.conn().unchecked_transaction()?;
        let trip = db
            .get_trip(&id)?
            .ok_or_else(|| anyhow::anyhow!("Trip not found"))?;
        ensure!(
            trip.updated_at == input.expected_updated_at,
            "Trip changed; refresh before editing"
        );
        let trip = db.update_trip(
            &id,
            &envelope_email_store::TripUpdate {
                title: input.title,
                destination: input.destination,
                starts_at: input.starts_at,
                ends_at: input.ends_at,
                timezone: input.timezone,
                status: input.status,
                notes: input.notes,
                updated_at: chrono::Utc::now().to_rfc3339(),
            },
        )?;
        tx.commit()?;
        Ok(json!({"trip":trip}))
    })())
}

#[derive(Deserialize)]
pub struct MoveBookings {
    pub destination_trip: String,
    pub booking_ids: Vec<String>,
}
pub async fn move_bookings(
    State(state): State<AppState>,
    Path(source): Path<String>,
    Json(input): Json<MoveBookings>,
) -> Response {
    let mut db = state.db.lock().await;
    response((|| {
        ensure!(source != input.destination_trip, "Choose a different trip");
        ensure!(
            db.get_trip(&source)?.is_some() && db.get_trip(&input.destination_trip)?.is_some(),
            "Trip not found"
        );
        ensure!(!input.booking_ids.is_empty(), "Select bookings");
        let tx = db.conn_mut().transaction()?;
        for id in input.booking_ids {
            let moved=tx.execute("UPDATE travel_bookings SET trip_id=?1,updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?2 AND trip_id=?3",rusqlite::params![input.destination_trip,id,source])?;
            ensure!(moved == 1, "Booking not in source trip");
            tx.execute(
                "UPDATE travel_segments SET trip_id=?1 WHERE booking_id=?2",
                rusqlite::params![input.destination_trip, id],
            )?;
            tx.execute("UPDATE travel_receipts SET trip_id=?1 WHERE id IN (SELECT receipt_id FROM travel_bookings WHERE id=?2)",rusqlite::params![input.destination_trip,id])?;
            tx.execute(
                "UPDATE travel_alerts SET trip_id=?1 WHERE booking_id=?2",
                rusqlite::params![input.destination_trip, id],
            )?;
        }
        tx.commit()?;
        Ok(json!({"status":"moved"}))
    })())
}

#[derive(Deserialize)]
pub struct Search {
    pub q: Option<String>,
}
pub async fn search(State(state): State<AppState>, Query(query): Query<Search>) -> Response {
    let db = state.db.lock().await;
    response((|| {
        let needle = query.q.unwrap_or_default().to_lowercase();
        let trips: Vec<_> = db
            .list_trips()?
            .into_iter()
            .filter(|trip| {
                format!(
                    "{} {}",
                    trip.title,
                    trip.destination.as_deref().unwrap_or_default()
                )
                .to_lowercase()
                .contains(&needle)
            })
            .collect();
        Ok(json!({"trips":trips}))
    })())
}

#[derive(Serialize, Deserialize)]
pub struct NativeItem {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub start: Option<String>,
    pub end: Option<String>,
    pub url: Option<String>,
    pub notes: String,
    pub status: String,
    pub updated_at: String,
    pub completed: bool,
    pub alarm_minutes: Option<u32>,
}

pub fn project(db: &Database, trip_filter: Option<&str>) -> Result<Vec<NativeItem>> {
    initialize(db)?;
    let mut items = Vec::new();
    for trip in db.list_trips()? {
        if trip_filter.is_some_and(|id| id != trip.id) {
            continue;
        }
        let segments = db.list_travel_segments(&trip.id)?;
        for booking in db.list_travel_bookings(&trip.id)? {
            let mut stmt = db.conn().prepare(
                "SELECT label,url FROM travel_action_links WHERE booking_id=?1 ORDER BY id",
            )?;
            let links = stmt
                .query_map([&booking.id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let url = links.first().map(|(_, url)| url.clone());
            let notes = links
                .iter()
                .map(|(label, url)| format!("{label}: {url}"))
                .collect::<Vec<_>>()
                .join("\n");
            let legs: Vec<_> = segments
                .iter()
                .filter(|segment| segment.booking_id.as_deref() == Some(&booking.id))
                .collect();
            if legs.is_empty() {
                items.push(NativeItem {
                    id: booking.id,
                    kind: "event".into(),
                    title: booking.title,
                    start: booking.starts_at,
                    end: booking.ends_at,
                    url,
                    notes,
                    status: booking.status,
                    updated_at: booking.updated_at,
                    completed: false,
                    alarm_minutes: if ["flight", "train", "car"].contains(&booking.kind.as_str()) {
                        Some(120)
                    } else {
                        None
                    },
                });
            } else {
                for leg in legs {
                    items.push(NativeItem {
                        id: leg.id.clone(),
                        kind: "event".into(),
                        title: if leg.origin.is_some() && leg.destination.is_some() { format!(
                            "{} · {} → {}",
                            leg.service_number.as_deref().unwrap_or(&booking.title),
                            leg.origin.as_deref().unwrap_or(""),
                            leg.destination.as_deref().unwrap_or("")
                        ) } else { booking.title.clone() },
                        start: leg.departs_at.clone(),
                        end: leg.arrives_at.clone(),
                        url: url.clone(),
                        notes: notes.clone(),
                        status: leg.status.clone(),
                        updated_at: leg.updated_at.clone(),
                        completed: false,
                        alarm_minutes: if ["flight","train","car"].contains(&booking.kind.as_str()) {Some(120)}else{None},
                    });
                }
            }
        }
    }
    for task in db.list_travel_tasks(trip_filter)? {
        items.push(NativeItem {
            id: task.id,
            kind: "reminder".into(),
            title: task.title,
            start: task.due_at,
            end: None,
            url: None,
            notes: task.notes.unwrap_or_default(),
            status: "active".into(),
            updated_at: task.updated_at,
            completed: task.completed_at.is_some(),
            alarm_minutes: Some(0),
        });
    }
    Ok(items)
}

pub async fn native(State(state): State<AppState>) -> Response {
    let db = state.db.lock().await;
    response(project(&db, None).map(
        |items| json!({"version":1,"items":items,"generated_at":chrono::Utc::now().to_rfc3339()}),
    ))
}

pub async fn calendar(State(state): State<AppState>) -> Response {
    let db = state.db.lock().await;
    match project(&db, None) {
        Ok(items) => {
            let events: Vec<_> = items
                .into_iter()
                .filter(|i| i.kind == "event")
                .filter_map(|item| {
                    Some(TravelCalendarEvent {
                        id: item.id,
                        summary: item.title,
                        description: Some(item.notes),
                        location: None,
                        start_at: item.start?,
                        end_at: item.end,
                        timezone: None,
                        status: item.status,
                        sequence: chrono::DateTime::parse_from_rfc3339(&item.updated_at)
                            .map(|d| d.timestamp().clamp(0, u32::MAX as i64) as u32)
                            .unwrap_or(0),
                        updated_at: Some(item.updated_at),
                        url: item.url,
                        alarm_minutes: item.alarm_minutes,
                    })
                })
                .collect();
            (
                [
                    (header::CONTENT_TYPE, "text/calendar; charset=utf-8"),
                    (header::CACHE_CONTROL, "private, no-store"),
                    (
                        header::CONTENT_DISPOSITION,
                        "attachment; filename=travel.ics",
                    ),
                ],
                render_travel_calendar("Travel", &events),
            )
                .into_response()
        }
        Err(error) => response(Err(error)),
    }
}

#[derive(Deserialize)]
pub struct Completion {
    pub completed: bool,
    pub expected_updated_at: String,
}
pub async fn complete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<Completion>,
) -> Response {
    let db = state.db.lock().await;
    response((|| {
        let changed = db.conn().execute(
            "UPDATE travel_tasks SET completed_at=?1,updated_at=?2 WHERE id=?3 AND updated_at=?4",
            rusqlite::params![
                if input.completed {
                    Some(chrono::Utc::now().to_rfc3339())
                } else {
                    None
                },
                chrono::Utc::now().to_rfc3339(),
                id,
                input.expected_updated_at
            ],
        )?;
        ensure!(
            changed == 1,
            "Task changed; refresh before applying completion"
        );
        Ok(json!({"status":"updated"}))
    })())
}
