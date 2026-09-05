//! Household bearer capabilities and browser sessions, with explicit trip scope.
use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use envelope_email_store::Database;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

#[derive(Clone, Serialize, Deserialize)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub role: String,
    pub trips: Vec<String>,
}
fn hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}
pub fn initialize(db: &Database) -> anyhow::Result<()> {
    db.conn().execute_batch("CREATE TABLE IF NOT EXISTS household_members(id TEXT PRIMARY KEY,name TEXT NOT NULL,role TEXT NOT NULL,trips TEXT NOT NULL,token_hash TEXT NOT NULL UNIQUE,revoked INTEGER NOT NULL DEFAULT 0);
    CREATE TABLE IF NOT EXISTS household_sessions(token_hash TEXT PRIMARY KEY,member_id TEXT,expires INTEGER NOT NULL);")?;
    Ok(())
}
fn denied() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error":"sign_in_required"})),
    )
        .into_response()
}

pub async fn authorize(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned);
    let cookie = request
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.split(';')
                .find_map(|p| p.trim().strip_prefix("travel_session="))
        })
        .map(str::to_owned);
    let owner_authorized = state.auth.bearer_authorized(request.headers());
    let resolution:Result<Option<Member>,()>=async {
        if owner_authorized{return Ok(None)}
        let db=state.db.lock().await;initialize(&db).map_err(|_|())?;
        if let Some(token)=presented.as_deref(){
            let row=db.conn().query_row("SELECT id,name,role,trips FROM household_members WHERE token_hash=?1 AND revoked=0",[hash(token)],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?))).optional().map_err(|_|())?;
            if let Some((id,name,role,trips))=row{return Ok(Some(Member{id,name,role,trips:serde_json::from_str(&trips).map_err(|_|())?}))}
            return Err(())
        }
        if let Some(token)=cookie {
            let member:Option<Option<String>>=db.conn().query_row("SELECT member_id FROM household_sessions WHERE token_hash=?1 AND expires>?2",rusqlite::params![hash(&token),chrono::Utc::now().timestamp()],|r|r.get(0)).optional().map_err(|_|())?;
            if let Some(member)=member{
                if let Some(id)=member{
                    return db.conn().query_row("SELECT name,role,trips FROM household_members WHERE id=?1 AND revoked=0",[&id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))).map_err(|_|()).and_then(|(name,role,trips)|Ok(Some(Member{id,name,role,trips:serde_json::from_str(&trips).map_err(|_|())?})));
                }
                return Ok(None)
            }
            return Err(())
        }
        if !state.auth.is_enforced(){return Ok(None)}
        Err(())
    }.await;
    match resolution {
        Err(()) => denied(),
        Ok(member) => {
            if let Some(member) = member {
                let path = request.uri().path();
                // Members use the scoped API. Aggregate owner endpoints never
                // silently return other household members' private receipts.
                if path != "/api/csrf"
                    && path != "/csrf"
                    && !path.starts_with("/api/v1/household/")
                    && !path.starts_with("/v1/household/")
                {
                    return StatusCode::FORBIDDEN.into_response();
                }
                request.extensions_mut().insert(member);
            }
            if presented.is_some() {
                request
                    .extensions_mut()
                    .insert(crate::auth::BearerAuthenticated);
            }
            next.run(request).await
        }
    }
}

pub async fn session(State(state): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
    else {
        return denied();
    };
    let mut db = state.db.lock().await;
    if initialize(&db).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let Ok(tx) = db
        .conn_mut()
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
    else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let member = if state.auth.bearer_authorized(&headers) {
        None
    } else {
        match tx
            .query_row(
                "SELECT id FROM household_members WHERE token_hash=?1 AND revoked=0",
                [hash(token)],
                |r| r.get::<_, String>(0),
            )
            .optional()
        {
            Ok(Some(id)) => Some(id),
            _ => return denied(),
        }
    };
    let session = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    if tx
        .execute(
            "INSERT INTO household_sessions VALUES(?1,?2,?3)",
            rusqlite::params![
                hash(&session),
                member,
                chrono::Utc::now().timestamp() + 86400 * 30
            ],
        )
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if let Some(id) = &member {
        // Consume enrollment within the same transaction that creates the session.
        if tx
            .execute(
                "UPDATE household_members SET token_hash=?2 WHERE id=?1",
                rusqlite::params![id, hash(&uuid::Uuid::new_v4().to_string())],
            )
            .is_err()
        {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    if tx.commit().is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let secure = headers
        .get("x-forwarded-proto")
        .is_some_and(|v| v == "https");
    let cookie = format!(
        "travel_session={session}; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000{}",
        if secure { "; Secure" } else { "" }
    );
    ([(header::SET_COOKIE,cookie)],Json(json!({"status":"signed_in","destination":if member.is_some(){"/household"}else{"/travel"}}))).into_response()
}

#[derive(Deserialize)]
pub struct Invitation {
    pub name: String,
    pub role: String,
    pub trips: Vec<String>,
}
pub async fn invite(State(state): State<AppState>, Json(input): Json<Invitation>) -> Response {
    if !["editor", "viewer"].contains(&input.role.as_str()) || input.name.trim().is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let db = state.db.lock().await;
    if initialize(&db).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    for id in &input.trips {
        if !matches!(db.get_trip(id), Ok(Some(_))) {
            return StatusCode::BAD_REQUEST.into_response();
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    match db.conn().execute(
        "INSERT INTO household_members(id,name,role,trips,token_hash) VALUES(?1,?2,?3,?4,?5)",
        rusqlite::params![
            id,
            input.name,
            input.role,
            json!(input.trips).to_string(),
            hash(&token)
        ],
    ) {
        Ok(_) => Json(json!({"id":id,"enrollment_path":format!("/household#token={token}")}))
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
pub async fn revoke(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let db = state.db.lock().await;
    match db
        .conn()
        .execute("UPDATE household_members SET revoked=1 WHERE id=?1", [id])
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => StatusCode::BAD_REQUEST.into_response(),
    }
}
pub async fn overview(
    State(state): State<AppState>,
    member: Option<axum::Extension<Member>>,
) -> Response {
    let Some(axum::Extension(member)) = member else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let db = state.db.lock().await;
    let mut trips = Vec::new();
    let mut bookings = Vec::new();
    let mut tasks = Vec::new();
    for id in &member.trips {
        if let Ok(Some(trip)) = db.get_trip(id) {
            trips.push(json!({"id":trip.id,"title":trip.title,"destination":trip.destination,"updated_at":trip.updated_at}));
        }
        if let Ok(rows) = db.list_travel_bookings(id) {
            for row in rows {
                bookings.push(json!({"id":row.id,"trip_id":row.trip_id,"title":row.title,"starts_at":row.starts_at,"ends_at":row.ends_at,"status":row.status}));
            }
        }
        if let Ok(rows) = db.list_travel_tasks(Some(id)) {
            tasks.extend(rows);
        }
    }
    Json(json!({"member":member,"trips":trips,"bookings":bookings,"tasks":tasks})).into_response()
}
#[derive(Deserialize)]
pub struct EditTrip {
    pub title: String,
    pub expected_updated_at: String,
}
pub async fn edit(
    State(state): State<AppState>,
    axum::Extension(member): axum::Extension<Member>,
    Path(id): Path<String>,
    Json(input): Json<EditTrip>,
) -> Response {
    if member.role != "editor" || !member.trips.contains(&id) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if input.title.trim().is_empty() || input.title.len() > 500 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let db = state.db.lock().await;
    match db.conn().execute(
        "UPDATE travel_trips SET title=?1,updated_at=?2 WHERE id=?3 AND updated_at=?4",
        rusqlite::params![
            input.title,
            chrono::Utc::now().to_rfc3339(),
            id,
            input.expected_updated_at
        ],
    ) {
        Ok(1) => StatusCode::NO_CONTENT.into_response(),
        _ => StatusCode::CONFLICT.into_response(),
    }
}
pub async fn toggle(
    State(state): State<AppState>,
    axum::Extension(member): axum::Extension<Member>,
    Path(id): Path<String>,
) -> Response {
    if member.role != "editor" {
        return StatusCode::FORBIDDEN.into_response();
    }
    let db = state.db.lock().await;
    match db.get_travel_task(&id) {
        Ok(Some(task))
            if task
                .trip_id
                .as_ref()
                .is_some_and(|id| member.trips.contains(id)) => {}
        _ => return StatusCode::FORBIDDEN.into_response(),
    }
    match db.toggle_travel_task(&id, &chrono::Utc::now().to_rfc3339()) {
        Ok(Some(task)) => Json(json!(task)).into_response(),
        _ => StatusCode::BAD_REQUEST.into_response(),
    }
}
