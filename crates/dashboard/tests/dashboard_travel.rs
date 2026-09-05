// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2

//! End-to-end contract tests for the local Travel workspace and its narrow
//! family-sharing capability surface.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use envelope_email_dashboard::dashboard_router;
use envelope_email_dashboard::state::AppState;
use envelope_email_store::{
    CredentialBackend, Database, NewTravelAlert, NewTravelBooking, NewTravelReceipt,
    NewTravelSegment, NewTravelShareLink, NewTravelTask, NewTrip,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const NOW: &str = "2026-08-31T12:00:00Z";

#[tokio::test]
async fn unauthenticated_local_instance_rejects_dns_rebinding_hosts(){
    let app=dashboard_router(fixture().state);
    for host in ["attacker.example","localhost.attacker.example","127.0.0.1.attacker.example"]{
        assert_eq!(request(app.clone(),Request::builder().uri("/api/travel/overview").header(header::HOST,host).body(Body::empty()).unwrap()).await.0,StatusCode::FORBIDDEN);
    }
    assert_eq!(request(app,Request::builder().uri("/api/travel/overview").header(header::HOST,"127.0.0.1:3150").body(Body::empty()).unwrap()).await.0,StatusCode::OK);
}

#[tokio::test]
async fn owner_cancellation_updates_native_projection_and_rejects_stale_edits() {
    use envelope_email_dashboard::auth::AuthConfig;
    let fixture = fixture();
    let state = fixture.state.with_auth(AuthConfig::from_parts(
        Some("owner-test-token".into()),
        Vec::new(),
    ));
    let app = dashboard_router(state.clone());
    let edit = || {
        Request::builder().method("PUT").uri("/api/v1/bookings/booking-flight")
        .header(header::AUTHORIZATION,"Bearer owner-test-token").header(header::CONTENT_TYPE,"application/json")
        .body(Body::from(json!({"expected_updated_at":NOW,"title":"Flight to Paris","status":"cancelled","starts_at":"2026-09-10T08:00:00Z","ends_at":"2026-09-10T10:00:00Z","location":"JFK"}).to_string())).unwrap()
    };
    assert_eq!(request(app.clone(), edit()).await.0, StatusCode::OK);
    assert_eq!(
        request(app.clone(), edit()).await.0,
        StatusCode::BAD_REQUEST
    );
    let db = state.db.lock().await;
    let items = envelope_email_dashboard::workspace::project(&db, None).unwrap();
    assert_eq!(
        items
            .iter()
            .find(|i| i.id == "segment-flight")
            .unwrap()
            .status,
        "cancelled"
    );
    assert!(
        !db.list_travel_alerts(None)
            .unwrap()
            .iter()
            .any(|a| a.booking_id.as_deref() == Some("booking-flight")
                && matches!(a.kind.as_str(), "check_in" | "upcoming"))
    );
}

#[tokio::test]
async fn household_enrollment_is_single_use_and_sessions_remain_scoped() {
    use envelope_email_dashboard::auth::AuthConfig;
    let fixture = fixture();
    let state = fixture.state.with_auth(AuthConfig::from_parts(
        Some("owner-test-token".into()),
        Vec::new(),
    ));
    let app = dashboard_router(state.clone());
    let (status, _, body) = request(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri("/api/v1/members")
            .header(header::AUTHORIZATION, "Bearer owner-test-token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({"name":"Family","role":"viewer","trips":["trip-paris"]}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let invitation: Value = serde_json::from_slice(&body).unwrap();
    let token = invitation["enrollment_path"]
        .as_str()
        .unwrap()
        .split("#token=")
        .nth(1)
        .unwrap();
    let login = || {
        Request::builder()
            .method("POST")
            .uri("/api/v1/session")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };
    let (status, headers, _) = request(app.clone(), login()).await;
    assert_eq!(status, StatusCode::OK);
    let cookie = headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    assert_eq!(
        request(app.clone(), login()).await.0,
        StatusCode::UNAUTHORIZED
    );
    for path in [
        "/api/travel/overview",
        "/api/v1/intelligence",
        "/api/v1/native/projection",
        "/api/v1/parsers",
    ] {
        assert_eq!(
            request(
                app.clone(),
                Request::builder()
                    .uri(path)
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    let (status, _, body) = request(
        app.clone(),
        Request::builder()
            .uri("/api/v1/household/overview")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(!body.contains("PRIVATE trip note"));
    assert!(!body.contains("SECRET42"));
    let overview: serde_json::Value = serde_json::from_str(&body).unwrap();
    let trips = overview["trips"].as_array().unwrap();
    assert_eq!(trips.len(), 1);
    assert_eq!(trips[0]["id"], "trip-paris");
    let tasks = overview["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], "task-passports");
    assert_eq!(tasks[0]["trip_id"], "trip-paris");
    assert!(!body.contains("trip-london"));
    assert!(!body.contains("task-london"));
    let id = invitation["id"].as_str().unwrap();
    assert_eq!(
        request(
            app.clone(),
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/members/{id}"))
                .header(header::AUTHORIZATION, "Bearer owner-test-token")
                .body(Body::empty())
                .unwrap()
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(
            app,
            Request::builder()
                .uri("/api/v1/household/overview")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap()
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

struct TravelFixture {
    state: AppState,
    editable_token: String,
    editable_calendar_token: String,
    view_only_token: String,
    editable_share_id: String,
}

fn fixture() -> TravelFixture {
    let db = Database::open_memory().unwrap();
    db.create_trip(&NewTrip {
        id: "trip-paris".into(),
        title: "Paris together".into(),
        destination: Some("Paris".into()),
        starts_at: Some("2026-09-10T08:00:00Z".into()),
        ends_at: Some("2026-09-15T18:00:00Z".into()),
        timezone: "Europe/Paris".into(),
        status: "upcoming".into(),
        notes: Some("PRIVATE trip note".into()),
        now: NOW.into(),
    })
    .unwrap();
    db.create_travel_booking(&NewTravelBooking {
        id: "booking-short-code".into(),
        trip_id: "trip-paris".into(),
        receipt_id: None,
        kind: "activity".into(),
        provider: Some("Museum Test".into()),
        title: "Museum visit".into(),
        confirmation_code: Some("A1B2".into()),
        status: "confirmed".into(),
        starts_at: Some("2026-09-12T09:00:00Z".into()),
        ends_at: Some("2026-09-12T11:00:00Z".into()),
        location: Some("Paris".into()),
        details: None,
        now: NOW.into(),
    })
    .unwrap();
    db.create_travel_booking(&NewTravelBooking {
        id: "booking-flight".into(),
        trip_id: "trip-paris".into(),
        receipt_id: None,
        kind: "flight".into(),
        provider: Some("Air Test".into()),
        title: "Flight to Paris".into(),
        confirmation_code: Some("SECRET42".into()),
        status: "confirmed".into(),
        starts_at: Some("2026-09-10T08:00:00Z".into()),
        ends_at: Some("2026-09-10T10:00:00Z".into()),
        location: Some("JFK Terminal 4".into()),
        details: Some(json!({
            "confirmation_code": "SECRET42",
            "private_receipt_note": "PRIVATE booking detail"
        })),
        now: NOW.into(),
    })
    .unwrap();
    db.create_travel_segment(&NewTravelSegment {
        id: "segment-flight".into(),
        trip_id: "trip-paris".into(),
        booking_id: Some("booking-flight".into()),
        kind: "flight".into(),
        sequence: 0,
        origin: Some("JFK".into()),
        destination: Some("CDG".into()),
        carrier: Some("Air Test".into()),
        service_number: Some("AT100".into()),
        departs_at: Some("2026-09-10T08:00:00Z".into()),
        arrives_at: Some("2026-09-10T10:00:00Z".into()),
        status: "confirmed".into(),
        details: Some(json!({ "seat": "PRIVATE 12A" })),
        now: NOW.into(),
    })
    .unwrap();
    db.create_travel_task(&NewTravelTask {
        id: "task-passports".into(),
        trip_id: Some("trip-paris".into()),
        title: "Check passports".into(),
        notes: Some("PRIVATE task note".into()),
        due_at: Some("2026-09-01T09:00:00Z".into()),
        now: NOW.into(),
    })
    .unwrap();
    db.create_trip(&NewTrip {
        id: "trip-london".into(),
        title: "London later".into(),
        destination: Some("London".into()),
        starts_at: Some("2026-10-10T08:00:00Z".into()),
        ends_at: Some("2026-10-12T18:00:00Z".into()),
        timezone: "Europe/London".into(),
        status: "upcoming".into(),
        notes: None,
        now: NOW.into(),
    })
    .unwrap();
    db.create_travel_task(&NewTravelTask {
        id: "task-london".into(),
        trip_id: Some("trip-london".into()),
        title: "Pack an umbrella".into(),
        notes: None,
        due_at: None,
        now: NOW.into(),
    })
    .unwrap();
    db.enqueue_travel_alert(&NewTravelAlert {
        id: "alert-checkin".into(),
        trip_id: Some("trip-paris".into()),
        booking_id: Some("booking-flight".into()),
        segment_id: Some("segment-flight".into()),
        dedupe_key: "flight:AT100:checkin".into(),
        kind: "check_in".into(),
        severity: "attention".into(),
        title: "Check in for AT100".into(),
        body: Some("PRIVATE alert body".into()),
        scheduled_at: Some("2026-09-09T08:00:00Z".into()),
        now: NOW.into(),
    })
    .unwrap();

    let editable = db
        .create_travel_share_link(&NewTravelShareLink {
            id: "share-editable".into(),
            trip_id: "trip-paris".into(),
            label: Some("Family".into()),
            can_edit_tasks: true,
            expires_at: None,
            created_at: NOW.into(),
        })
        .unwrap();
    let view_only = db
        .create_travel_share_link(&NewTravelShareLink {
            id: "share-view-only".into(),
            trip_id: "trip-paris".into(),
            label: Some("Friends".into()),
            can_edit_tasks: false,
            expires_at: None,
            created_at: NOW.into(),
        })
        .unwrap();

    TravelFixture {
        state: AppState::new(db, CredentialBackend::File),
        editable_token: editable.token,
        editable_calendar_token: editable.calendar_token,
        view_only_token: view_only.token,
        editable_share_id: editable.link.id,
    }
}

async fn request(
    app: axum::Router,
    request: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, body)
}

#[tokio::test]
async fn protected_overview_masks_booking_secrets() {
    let fixture = fixture();
    let (status, _headers, body) = request(
        dashboard_router(fixture.state),
        Request::builder()
            .uri("/api/travel/overview")
            .header("host", "localhost:3141")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["gmail_onboarding_secure"], true);
    assert_eq!(value["bookings"][0]["confirmation_masked"], "•••• ET42");
    let short = value["bookings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|booking| booking["id"] == "booking-short-code")
        .unwrap();
    assert_eq!(short["confirmation_masked"], "••••");
    let serialized = String::from_utf8(body).unwrap();
    assert!(!serialized.contains("SECRET42"));
    assert!(!serialized.contains("A1B2"));
    assert!(!serialized.contains("PRIVATE booking detail"));
}

#[tokio::test]
async fn receipt_approval_explicit_nulls_clear_false_extractions() {
    let db = Database::open_memory().unwrap();
    db.ingest_travel_receipt(&NewTravelReceipt {
        id: "receipt-clear-fields".into(),
        account_id: "gmail".into(),
        folder: "INBOX".into(),
        uidvalidity: 7,
        uid: 42,
        message_id: Some("clear-fields@example.test".into()),
        from_addr: Some("Air Test <tickets@example.test>".into()),
        subject: Some("Flight confirmation".into()),
        received_at: Some("2026-08-31T08:00:00Z".into()),
        authenticated_sender_domain: Some("example.test".into()),
        body_text: Some("Record locator: SECRET42".into()),
        extracted: Some(json!({
            "kind": "flight",
            "title": "Flight to Paris",
            "provider": "Wrong Air",
            "confirmation_code": "SECRET42",
            "status": "confirmed",
            "start_at": "2026-09-10T08:00:00Z",
            "end_at": "2026-09-10T16:00:00Z",
            "timezone": "Europe/Paris",
            "origin": "WRONG",
            "destination": "ALSO-WRONG",
            "address": null,
            "service_number": "AT100",
            "amount_minor": null,
            "currency": null,
            "confidence": 0.7,
            "decision": "quarantined",
            "reasons": ["review"],
            "parser_version": "test"
        })),
        trip_id: None,
        quarantine_reason: Some("review".into()),
        ingested_at: NOW.into(),
    })
    .unwrap();
    let state = AppState::new(db, CredentialBackend::File);
    let app = dashboard_router(state.clone());

    let (status, _, body) = request(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri("/api/travel/receipts/receipt-clear-fields/approve")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, "travel_csrf=tok123")
            .header("x-travel-csrf", "tok123")
            .body(Body::from(
                json!({
                    "provider": null,
                    "confirmation_code": null,
                    "end_at": null,
                    "origin": null,
                    "destination": null
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert!(value["booking"]["provider"].is_null());
    assert!(value["booking"]["confirmation_masked"].is_null());
    assert!(value["booking"]["ends_at"].is_null());
    assert!(value["segment"]["origin"].is_null());
    assert!(value["segment"]["destination"].is_null());
    assert!(!String::from_utf8(body).unwrap().contains("SECRET42"));

    let receipt = state
        .db
        .lock()
        .await
        .get_travel_receipt("receipt-clear-fields")
        .unwrap()
        .unwrap();
    let extracted = receipt.extracted.unwrap();
    assert!(extracted["provider"].is_null());
    assert!(extracted["confirmation_code"].is_null());
    assert!(extracted["end_at"].is_null());
    assert!(extracted["origin"].is_null());
    assert!(extracted["destination"].is_null());

    let (status, _, overview_body) = request(
        app,
        Request::builder()
            .uri("/api/travel/overview")
            .header("host", "localhost:3141")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !String::from_utf8(overview_body)
            .unwrap()
            .contains("SECRET42")
    );
}

#[tokio::test]
async fn public_trip_and_calendar_are_structurally_redacted() {
    let fixture = fixture();
    let app = dashboard_router(fixture.state.clone());
    let (status, headers, body) = request(
        app.clone(),
        Request::builder()
            .uri(format!("/api/public/travel/{}", fixture.editable_token))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CACHE_CONTROL).unwrap(),
        "private, no-store"
    );
    let public_json = String::from_utf8(body).unwrap();
    assert!(public_json.contains("Paris together"));
    assert!(public_json.contains("Check passports"));
    assert!(public_json.contains(&fixture.editable_calendar_token));
    assert!(!public_json.contains(&fixture.editable_token));
    for secret in [
        "SECRET42",
        "A1B2",
        "PRIVATE trip note",
        "PRIVATE booking detail",
        "PRIVATE 12A",
        "PRIVATE task note",
        "PRIVATE alert body",
    ] {
        assert!(!public_json.contains(secret), "public JSON leaked {secret}");
    }

    let (status, headers, body) = request(
        app.clone(),
        Request::builder()
            .uri(format!(
                "/api/public/travel/{}/calendar.ics",
                fixture.editable_calendar_token
            ))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "text/calendar; charset=utf-8"
    );
    let calendar = String::from_utf8(body).unwrap();
    assert!(calendar.contains("BEGIN:VCALENDAR"));
    assert!(calendar.contains("UID:segment-segment-flight@envelope.travel"));
    assert!(calendar.contains("AT100"));
    assert!(!calendar.contains("SEQUENCE:0\r\n"));
    assert!(calendar.contains("LAST-MODIFIED:20260831T120000Z"));
    assert!(!calendar.contains("SECRET42"));
    assert!(!calendar.contains("PRIVATE"));

    let (status, _, _) = request(
        app.clone(),
        Request::builder()
            .uri(format!(
                "/api/public/travel/{}",
                fixture.editable_calendar_token
            ))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);

    let (status, _, _) = request(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri(format!(
                "/api/public/travel/{}/tasks/task-passports/toggle",
                fixture.editable_calendar_token
            ))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);

    let (status, _, _) = request(
        app,
        Request::builder()
            .uri(format!(
                "/api/public/travel/{}/calendar.ics",
                fixture.editable_token
            ))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
}

#[tokio::test]
async fn family_task_editing_is_scoped_and_share_links_revoke_closed() {
    let fixture = fixture();
    let app = dashboard_router(fixture.state.clone());

    let (status, _, _) = request(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri(format!(
                "/api/public/travel/{}/tasks/task-passports/toggle",
                fixture.view_only_token
            ))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _, _) = request(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri(format!(
                "/api/public/travel/{}/tasks/task-london/toggle",
                fixture.editable_token
            ))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    {
        let db = fixture.state.db.lock().await;
        assert!(
            db.get_travel_task("task-london")
                .unwrap()
                .unwrap()
                .completed_at
                .is_none()
        );
    }

    let (status, _, _) = request(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri(format!(
                "/api/public/travel/{}/tasks/task-passports/toggle",
                fixture.editable_calendar_token
            ))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);

    let (status, _, body) = request(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri(format!(
                "/api/public/travel/{}/tasks/task-passports/toggle",
                fixture.editable_token
            ))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert!(value["task"]["completed_at"].is_string());

    {
        let db = fixture.state.db.lock().await;
        db.revoke_travel_share_link(&fixture.editable_share_id, NOW)
            .unwrap();
    }
    let (status, _, _) = request(
        app.clone(),
        Request::builder()
            .uri(format!("/api/public/travel/{}", fixture.editable_token))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);

    let (status, _, _) = request(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri(format!(
                "/api/public/travel/{}/tasks/task-passports/toggle",
                fixture.editable_token
            ))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);

    let (status, _, _) = request(
        app,
        Request::builder()
            .uri(format!(
                "/api/public/travel/{}/calendar.ics",
                fixture.editable_calendar_token
            ))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
}

#[tokio::test]
async fn receipt_endpoints_conflict_instead_of_overwriting_terminal_state() {
    let fixture = fixture();
    {
        let db = fixture.state.db.lock().await;
        db.ingest_travel_receipt(&NewTravelReceipt {
            id: "receipt-processed".into(),
            account_id: "gmail".into(),
            folder: "INBOX".into(),
            uidvalidity: 7,
            uid: 101,
            message_id: Some("processed@example.test".into()),
            from_addr: Some("tickets@example.test".into()),
            subject: Some("Processed booking".into()),
            received_at: Some(NOW.into()),
            authenticated_sender_domain: None,
            body_text: Some("Processed receipt body".into()),
            extracted: None,
            trip_id: None,
            quarantine_reason: None,
            ingested_at: NOW.into(),
        })
        .unwrap();
        db.mark_travel_receipt_processed(
            "receipt-processed",
            Some("trip-paris"),
            Some(&json!({"kind": "flight"})),
            NOW,
        )
        .unwrap();
        db.ingest_travel_receipt(&NewTravelReceipt {
            id: "receipt-dismissed".into(),
            account_id: "gmail".into(),
            folder: "INBOX".into(),
            uidvalidity: 7,
            uid: 102,
            message_id: Some("dismissed@example.test".into()),
            from_addr: Some("tickets@example.test".into()),
            subject: Some("Dismissed booking".into()),
            received_at: Some(NOW.into()),
            authenticated_sender_domain: None,
            body_text: Some("Dismissed receipt body".into()),
            extracted: None,
            trip_id: None,
            quarantine_reason: Some("needs review".into()),
            ingested_at: NOW.into(),
        })
        .unwrap();
        db.dismiss_travel_receipt("receipt-dismissed", NOW).unwrap();
    }
    let app = dashboard_router(fixture.state.clone());

    let (status, _, body) = request(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri("/api/travel/receipts/receipt-processed/dismiss")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, "travel_csrf=travel-race")
            .header("x-travel-csrf", "travel-race")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["code"], "travel_receipt_state_conflict");
    assert_eq!(value["current_status"], "processed");

    let (status, _, body) = request(
        app,
        Request::builder()
            .method("POST")
            .uri("/api/travel/receipts/receipt-dismissed/approve")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, "travel_csrf=travel-race")
            .header("x-travel-csrf", "travel-race")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["code"], "travel_receipt_state_conflict");
    assert_eq!(value["current_status"], "dismissed");

    let db = fixture.state.db.lock().await;
    assert_eq!(
        db.get_travel_receipt("receipt-processed")
            .unwrap()
            .unwrap()
            .status,
        "processed"
    );
    assert_eq!(
        db.get_travel_receipt("receipt-dismissed")
            .unwrap()
            .unwrap()
            .status,
        "dismissed"
    );
}
