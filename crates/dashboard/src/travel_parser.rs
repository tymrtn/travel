// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Deterministic travel-document classification and extraction.
//!
//! This is deliberately a compiled workflow: known schemas and labelled fields
//! take the default path, ambiguous documents are quarantined for review, and
//! nothing asks an LLM to reinterpret every receipt. Parser changes are covered
//! by redacted fixtures below so a newly supported vendor cannot regress an old
//! one silently.

use std::sync::OnceLock;

use chrono::{NaiveDate, NaiveDateTime};
use mail_parser::MimeHeaders;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const TRAVEL_PARSER_VERSION: &str = "travel-rules-1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CandidateDecision {
    pub score: i32,
    pub state: String,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParsedTravelDocument {
    pub kind: String,
    pub title: String,
    pub provider: Option<String>,
    pub confirmation_code: Option<String>,
    pub status: String,
    pub start_at: Option<String>,
    pub end_at: Option<String>,
    pub timezone: Option<String>,
    pub origin: Option<String>,
    pub destination: Option<String>,
    pub address: Option<String>,
    pub service_number: Option<String>,
    pub amount_minor: Option<i64>,
    pub currency: Option<String>,
    pub confidence: f64,
    pub decision: String,
    pub reasons: Vec<String>,
    pub parser_version: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RawTravelEmail {
    pub message_id: Option<String>,
    pub from_addr: String,
    pub subject: String,
    pub received_at: Option<String>,
    /// RFC5322 From domain authenticated by Gmail's own topmost
    /// Authentication-Results header (DMARC pass + aligned header.from).
    /// Display names and sender-supplied authentication headers never qualify.
    pub authenticated_sender_domain: Option<String>,
    pub text_body: String,
    pub html_body: Option<String>,
    pub ics_body: Option<String>,
    pub attachment_names: Vec<String>,
}

/// Decode only the fields the travel pipeline needs from a canonical RFC822
/// source. Attachment bytes are never persisted by this path; calendar text is
/// accepted only under a small cap and every other attachment is represented
/// by its filename for review provenance.
pub fn parse_raw_travel_email(rfc822: &[u8]) -> Option<RawTravelEmail> {
    const MAX_BODY_CHARS: usize = 256 * 1024;
    const MAX_ICS_BYTES: usize = 256 * 1024;

    let parsed = mail_parser::MessageParser::default().parse(rfc822)?;
    let from_addr = first_address(parsed.from());
    let authenticated_sender_domain = first_address_domain(parsed.from())
        .and_then(|domain| gmail_authenticated_from_domain(&parsed, &domain));
    let subject = parsed.subject().unwrap_or_default().trim().to_string();
    let received_at = parsed.date().map(|date| date.to_rfc3339());
    let text_body = truncate_chars(
        parsed
            .body_text(0)
            .map(|body| body.to_string())
            .unwrap_or_default(),
        MAX_BODY_CHARS,
    );
    let html_body = parsed
        .body_html(0)
        .map(|body| truncate_chars(body.to_string(), MAX_BODY_CHARS));
    let mut ics_body = None;
    let mut attachment_names = Vec::new();
    for (index, attachment) in parsed.attachments().enumerate() {
        let name = attachment
            .attachment_name()
            .map(str::to_string)
            .unwrap_or_else(|| format!("attachment-{}", index + 1));
        let is_calendar = name.to_ascii_lowercase().ends_with(".ics")
            || attachment.content_type().is_some_and(|ct| {
                ct.ctype().eq_ignore_ascii_case("text") && ct.subtype() == Some("calendar")
            });
        if is_calendar && attachment.contents().len() <= MAX_ICS_BYTES && ics_body.is_none() {
            ics_body = Some(String::from_utf8_lossy(attachment.contents()).into_owned());
        }
        attachment_names.push(name);
    }

    Some(RawTravelEmail {
        message_id: parsed.message_id().map(str::to_string),
        from_addr,
        subject,
        received_at,
        authenticated_sender_domain,
        text_body,
        html_body,
        ics_body,
        attachment_names,
    })
}

fn gmail_authenticated_from_domain(
    message: &mail_parser::Message<'_>,
    from_domain: &str,
) -> Option<String> {
    static DMARC_FROM: OnceLock<Regex> = OnceLock::new();
    let raw = message
        .headers_raw()
        .find(|(name, _)| name.eq_ignore_ascii_case("authentication-results"))?
        .1;
    let unfolded = raw.replace(['\r', '\n', '\t'], " ");
    let mut parts = unfolded.split(';');
    let authserv_id = parts.next()?.split_whitespace().next()?;
    if !authserv_id.eq_ignore_ascii_case("mx.google.com") {
        return None;
    }

    let dmarc = parts.find(|part| {
        part.to_ascii_lowercase()
            .split_whitespace()
            .any(|token| token.trim().eq_ignore_ascii_case("dmarc=pass"))
    })?;
    let authenticated_domain = re(
        r"(?i)\bheader\.from\s*=\s*([a-z0-9](?:[a-z0-9.-]{0,251}[a-z0-9])?)\.?(?:\s|$)",
        &DMARC_FROM,
    )
    .captures(dmarc)?
    .get(1)?
    .as_str()
    .to_ascii_lowercase();
    authenticated_domain
        .eq_ignore_ascii_case(from_domain)
        .then_some(authenticated_domain)
}

impl ParsedTravelDocument {
    fn unknown(subject: &str, reasons: Vec<String>) -> Self {
        Self {
            kind: "unknown".into(),
            title: clean_subject(subject),
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
            reasons,
            parser_version: TRAVEL_PARSER_VERSION.into(),
        }
    }
}

fn re(pattern: &'static str, cell: &'static OnceLock<Regex>) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("travel parser regex must compile"))
}

fn lower_haystack(from: &str, subject: &str, body: &str) -> String {
    format!("{from}\n{subject}\n{body}").to_lowercase()
}

/// Score only enough metadata to decide whether fetching a complete MIME body
/// is justified. A score below four stays out of the travel pipeline.
pub fn classify_candidate(from: &str, subject: &str, body_hint: &str) -> CandidateDecision {
    static KNOWN: OnceLock<Regex> = OnceLock::new();
    static INTENT: OnceLock<Regex> = OnceLock::new();
    static CHANGE: OnceLock<Regex> = OnceLock::new();
    static ENTITY: OnceLock<Regex> = OnceLock::new();
    static MARKETING: OnceLock<Regex> = OnceLock::new();
    static STRUCTURED: OnceLock<Regex> = OnceLock::new();
    static CONFIRMATION_LABEL: OnceLock<Regex> = OnceLock::new();

    let hay = lower_haystack(from, subject, body_hint);
    let mut score = 0;
    let mut reasons = Vec::new();

    if re(
        r"(?i)(airbnb|booking\.com|expedia|trip\.com|united|delta|aa\.com|american airlines|jetblue|southwest|lufthansa|air france|klm|ryanair|easyjet|iberia|vueling|trainline|amtrak|sncf|renfe|hertz|avis|enterprise|marriott|hilton|hyatt|hotels\.com)",
        &KNOWN,
    )
    .is_match(&hay)
    {
        score += 5;
        reasons.push("recognized_travel_sender".into());
    }
    if re(
        r"(?i)\b(booking|reservation|confirmation|confirmed|itinerary|e-ticket|boarding pass|voucher|rental receipt|hotel receipt|flight receipt)\b",
        &INTENT,
    )
    .is_match(&hay)
    {
        score += 3;
        reasons.push("booking_language".into());
    }
    if re(
        r"(?i)\b(schedule change|flight change|booking changed|cancelled|canceled|cancellation)\b",
        &CHANGE,
    )
    .is_match(&hay)
    {
        score += 4;
        reasons.push("change_or_cancellation".into());
    }
    if re(
        r"(?i)\b(flight|hotel|check-in|check out|departure|arrival|train|rail|rental car|pickup|drop-off|tour|activity)\b",
        &ENTITY,
    )
    .is_match(&hay)
    {
        score += 2;
        reasons.push("travel_entities".into());
    }
    if re(
        r"(?i)(application/ld\+json|flightreservation|lodgingreservation|trainreservation|rentalcarreservation|begin:vcalendar)",
        &STRUCTURED,
    )
    .is_match(&hay)
    {
        score += 8;
        reasons.push("structured_reservation".into());
    }
    if re(
        r"(?i)\b(newsletter|fare sale|flash sale|travel inspiration|deal alert|points promotion|unsubscribe)\b",
        &MARKETING,
    )
    .is_match(&hay)
        && !re(
            r"(?i)\b(confirmation|reservation number|record locator)\b",
            &CONFIRMATION_LABEL,
        )
        .is_match(&hay)
    {
        score -= 6;
        reasons.push("marketing_signals".into());
    }

    let state = if score >= 8 {
        "parse"
    } else if score >= 3 {
        "review"
    } else {
        "ignored"
    };
    CandidateDecision {
        score,
        state: state.into(),
        reasons,
    }
}

pub fn parse_travel_document(
    from: &str,
    subject: &str,
    _received_at: Option<&str>,
    text_body: &str,
    html_body: Option<&str>,
    ics_body: Option<&str>,
) -> ParsedTravelDocument {
    let candidate = classify_candidate(from, subject, text_body);
    let html = html_body.unwrap_or_default();
    let plain = if text_body.trim().is_empty() {
        strip_html(html)
    } else {
        text_body.to_string()
    };
    let combined = format!("{subject}\n{plain}");

    let mut parsed = ParsedTravelDocument::unknown(subject, candidate.reasons.clone());
    parsed.provider = provider_from_sender(from);

    let structured = parse_json_ld(html).or_else(|| ics_body.and_then(parse_ics));
    let had_structured = structured.is_some();
    if let Some(structured) = structured {
        merge_structured(&mut parsed, structured);
        parsed.reasons.push("structured_fields_extracted".into());
    }

    if parsed.kind == "unknown" {
        parsed.kind = infer_kind(&combined).into();
    }
    parsed.confirmation_code = parsed
        .confirmation_code
        .or_else(|| labelled_confirmation(&combined));
    parsed.service_number = parsed
        .service_number
        .or_else(|| labelled_service_number(&combined, &parsed.kind));
    parsed.origin = parsed
        .origin
        .or_else(|| labelled_value(&combined, "origin|from|departing"));
    parsed.destination = parsed.destination.or_else(|| {
        labelled_value(
            &combined,
            "destination|to|arriving|arrival airport|hotel|property",
        )
    });
    parsed.address = parsed
        .address
        .or_else(|| labelled_value(&combined, "address|pickup location|property address"));
    let (start, end) = labelled_dates(&combined, &parsed.kind);
    parsed.start_at = parsed.start_at.or(start);
    parsed.end_at = parsed.end_at.or(end);
    let (amount, currency) = extract_amount(&combined);
    parsed.amount_minor = parsed.amount_minor.or(amount);
    parsed.currency = parsed.currency.or(currency);

    let inferred_status = infer_status(subject, &combined, parsed.confirmation_code.as_deref());
    if parsed.status == "unknown" || matches!(inferred_status.as_str(), "cancelled" | "changed") {
        parsed.status = inferred_status;
    }
    parsed.title = infer_title(subject, &parsed);

    let mut confidence: f64 = if had_structured { 0.48 } else { 0.20 };
    if parsed.kind != "unknown" {
        confidence += 0.14;
    }
    if parsed.confirmation_code.is_some() {
        confidence += 0.16;
    }
    if parsed.start_at.is_some() {
        confidence += 0.12;
    }
    if parsed.destination.is_some() || parsed.address.is_some() {
        confidence += 0.08;
    }
    if parsed.service_number.is_some() || parsed.provider.is_some() {
        confidence += 0.06;
    }
    if candidate.score >= 8 {
        confidence += 0.05;
    }
    if ambiguous_numeric_date(&combined) && !had_structured {
        confidence = confidence.min(0.69);
        parsed.reasons.push("ambiguous_numeric_date".into());
    }
    if candidate.state == "ignored" {
        confidence = confidence.min(0.25);
    }
    parsed.confidence = confidence.min(0.99);
    parsed.decision = if candidate.state == "ignored" {
        "ignored"
    } else if parsed.confidence >= 0.92 && required_fields_present(&parsed) {
        "auto_accepted"
    } else if parsed.confidence >= 0.60 {
        "review"
    } else {
        "quarantined"
    }
    .into();
    parsed
}

#[derive(Debug, Clone)]
struct StructuredFields {
    kind: String,
    title: Option<String>,
    provider: Option<String>,
    confirmation_code: Option<String>,
    status: Option<String>,
    start_at: Option<String>,
    end_at: Option<String>,
    origin: Option<String>,
    destination: Option<String>,
    address: Option<String>,
    service_number: Option<String>,
    amount_minor: Option<i64>,
    currency: Option<String>,
}

fn parse_json_ld(html: &str) -> Option<StructuredFields> {
    static JSON_LD: OnceLock<Regex> = OnceLock::new();
    let captures = re(
        r#"(?is)<script[^>]*type\s*=\s*[\"']application/ld\+json[\"'][^>]*>(.*?)</script>"#,
        &JSON_LD,
    )
    .captures_iter(html);
    for capture in captures {
        let raw = capture.get(1)?.as_str().trim();
        let Ok(value) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        if let Some(found) = find_reservation(&value) {
            return Some(found);
        }
    }
    None
}

fn find_reservation(value: &Value) -> Option<StructuredFields> {
    match value {
        Value::Array(items) => items.iter().find_map(find_reservation),
        Value::Object(map) => {
            if let Some(graph) = map.get("@graph")
                && let Some(found) = find_reservation(graph)
            {
                return Some(found);
            }
            let type_name = value_text(map.get("@type"))?.to_lowercase();
            let kind = if type_name.contains("flightreservation") {
                "flight"
            } else if type_name.contains("lodgingreservation") {
                "hotel"
            } else if type_name.contains("trainreservation") || type_name.contains("busreservation")
            {
                "rail"
            } else if type_name.contains("rentalcarreservation") {
                "car"
            } else if type_name.contains("reservation") {
                "activity"
            } else {
                return map.values().find_map(find_reservation);
            };
            let reserved = map.get("reservationFor").and_then(Value::as_object);
            let provider = object_text(map.get("provider"), "name")
                .or_else(|| reserved.and_then(|r| object_text(r.get("airline"), "name")))
                .or_else(|| reserved.and_then(|r| value_text(r.get("name"))));
            let title = reserved
                .and_then(|r| value_text(r.get("name")))
                .or_else(|| value_text(map.get("name")));
            let start_at = first_text(
                map,
                &["checkinTime", "pickupTime", "startTime", "departureTime"],
            )
            .or_else(|| {
                reserved.and_then(|r| first_text(r, &["departureTime", "checkinTime", "startDate"]))
            })
            .and_then(|v| normalize_datetime(&v));
            let end_at = first_text(
                map,
                &["checkoutTime", "dropoffTime", "endTime", "arrivalTime"],
            )
            .or_else(|| {
                reserved.and_then(|r| first_text(r, &["arrivalTime", "checkoutTime", "endDate"]))
            })
            .and_then(|v| normalize_datetime(&v));
            let origin = reserved.and_then(|r| {
                object_text(r.get("departureAirport"), "iataCode")
                    .or_else(|| object_text(r.get("departureStation"), "name"))
            });
            let destination = reserved.and_then(|r| {
                object_text(r.get("arrivalAirport"), "iataCode")
                    .or_else(|| object_text(r.get("arrivalStation"), "name"))
                    .or_else(|| value_text(r.get("name")))
            });
            let address = reserved.and_then(|r| {
                object_text(r.get("address"), "streetAddress")
                    .or_else(|| value_text(r.get("address")))
            });
            let service_number =
                reserved.and_then(|r| first_text(r, &["flightNumber", "trainNumber", "busNumber"]));
            let amount_text = value_text(map.get("totalPrice"));
            let amount_minor = amount_text.as_deref().and_then(parse_money_minor);
            Some(StructuredFields {
                kind: kind.into(),
                title,
                provider,
                confirmation_code: first_text(map, &["reservationNumber", "confirmationNumber"]),
                status: value_text(map.get("reservationStatus")),
                start_at,
                end_at,
                origin,
                destination,
                address,
                service_number,
                amount_minor,
                currency: value_text(map.get("priceCurrency")).map(|s| s.to_uppercase()),
            })
        }
        _ => None,
    }
}

fn parse_ics(ics: &str) -> Option<StructuredFields> {
    if !ics.to_ascii_uppercase().contains("BEGIN:VEVENT") {
        return None;
    }
    let value = |name: &str| -> Option<String> {
        ics.lines().find_map(|line| {
            let (head, body) = line.split_once(':')?;
            (head.split(';').next()?.eq_ignore_ascii_case(name)).then(|| body.trim().to_string())
        })
    };
    Some(StructuredFields {
        kind: "activity".into(),
        title: value("SUMMARY"),
        provider: value("ORGANIZER"),
        confirmation_code: value("UID"),
        status: value("STATUS"),
        start_at: value("DTSTART").and_then(|v| normalize_ics_datetime(&v)),
        end_at: value("DTEND").and_then(|v| normalize_ics_datetime(&v)),
        origin: None,
        destination: value("LOCATION"),
        address: value("LOCATION"),
        service_number: None,
        amount_minor: None,
        currency: None,
    })
}

fn normalize_ics_datetime(raw: &str) -> Option<String> {
    for fmt in ["%Y%m%dT%H%M%SZ", "%Y%m%dT%H%M%S", "%Y%m%d"] {
        if fmt == "%Y%m%d" {
            if let Ok(d) = NaiveDate::parse_from_str(raw, fmt) {
                return Some(format!("{}T00:00:00", d.format("%Y-%m-%d")));
            }
        } else if let Ok(dt) = NaiveDateTime::parse_from_str(raw, fmt) {
            let suffix = if raw.ends_with('Z') { "Z" } else { "" };
            return Some(format!("{}{suffix}", dt.format("%Y-%m-%dT%H:%M:%S")));
        }
    }
    None
}

fn merge_structured(parsed: &mut ParsedTravelDocument, fields: StructuredFields) {
    parsed.kind = fields.kind;
    if let Some(title) = fields.title {
        parsed.title = title;
    }
    parsed.provider = fields.provider.or(parsed.provider.take());
    parsed.confirmation_code = fields.confirmation_code;
    if let Some(status) = fields.status {
        parsed.status = normalize_status(&status);
    }
    parsed.start_at = fields.start_at;
    parsed.end_at = fields.end_at;
    parsed.origin = fields.origin;
    parsed.destination = fields.destination;
    parsed.address = fields.address;
    parsed.service_number = fields.service_number;
    parsed.amount_minor = fields.amount_minor;
    parsed.currency = fields.currency;
}

fn first_text(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| value_text(map.get(*key)))
}

fn value_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(items) => items.iter().find_map(|item| value_text(Some(item))),
        _ => None,
    }
}

fn object_text(value: Option<&Value>, key: &str) -> Option<String> {
    let value = value?;
    if let Some(text) = value_text(Some(value)) {
        return Some(text);
    }
    value.as_object().and_then(|m| value_text(m.get(key)))
}

fn infer_kind(text: &str) -> &'static str {
    static FLIGHT: OnceLock<Regex> = OnceLock::new();
    static HOTEL: OnceLock<Regex> = OnceLock::new();
    static RAIL: OnceLock<Regex> = OnceLock::new();
    static CAR: OnceLock<Regex> = OnceLock::new();
    static ACTIVITY: OnceLock<Regex> = OnceLock::new();
    static RECEIPT: OnceLock<Regex> = OnceLock::new();
    if re(
        r"(?i)\b(flight|airline|boarding|departure airport|arrival airport)\b",
        &FLIGHT,
    )
    .is_match(text)
    {
        "flight"
    } else if re(
        r"(?i)\b(hotel|lodging|check[ -]?in|check[ -]?out|property)\b",
        &HOTEL,
    )
    .is_match(text)
    {
        "hotel"
    } else if re(r"(?i)\b(train|rail|amtrak|station|coach)\b", &RAIL).is_match(text) {
        "rail"
    } else if re(
        r"(?i)\b(rental car|car rental|pickup location|drop[ -]?off)\b",
        &CAR,
    )
    .is_match(text)
    {
        "car"
    } else if re(
        r"(?i)\b(tour|activity|museum|experience|admission|voucher)\b",
        &ACTIVITY,
    )
    .is_match(text)
    {
        "activity"
    } else if re(r"(?i)\b(receipt|invoice|payment)\b", &RECEIPT).is_match(text) {
        "receipt"
    } else {
        "unknown"
    }
}

fn labelled_confirmation(text: &str) -> Option<String> {
    static CONFIRMATION: OnceLock<Regex> = OnceLock::new();
    let caps = re(
        r"(?im)\b(?:confirmation(?:[ \t]+(?:number|code))?|record locator|booking reference|reservation number|pnr)[ \t]*(?:#|no\.?|number|code|:|-)?[ \t]*([A-Z0-9][A-Z0-9-]{4,15})\b",
        &CONFIRMATION,
    )
    .captures(text)?;
    Some(caps.get(1)?.as_str().to_uppercase())
}

fn labelled_service_number(text: &str, kind: &str) -> Option<String> {
    let label = match kind {
        "flight" => "flight",
        "rail" => "train|service",
        _ => "service",
    };
    let pattern =
        format!(r"(?im)\b(?:{label})(?:\s+(?:number|no\.?))?\s*:?\s*([A-Z]{{1,3}}\s?\d{{1,5}})\b");
    let regex = Regex::new(&pattern).ok()?;
    regex.captures(text).and_then(|caps| caps.get(1)).map(|m| {
        m.as_str()
            .split_whitespace()
            .collect::<String>()
            .to_uppercase()
    })
}

fn labelled_value(text: &str, labels: &str) -> Option<String> {
    // A heading such as 'Hotel reservation confirmed' is not a Hotel field.
    let pattern = format!(r"(?im)^[ \t]*(?:{labels})[ \t]*[:–][ \t]*([^\r\n]{{2,120}})$");
    let regex = Regex::new(&pattern).ok()?;
    regex.captures(text).and_then(|caps| caps.get(1)).map(|m| {
        m.as_str()
            .trim()
            .trim_matches(|c: char| c == '-' || c == '–')
            .trim()
            .to_string()
    })
}

fn labelled_dates(text: &str, kind: &str) -> (Option<String>, Option<String>) {
    let start_labels = match kind {
        "hotel" => "check[ -]?in|arrival|start",
        "car" => "pick[ -]?up|start",
        _ => "departure|depart|start|date|when",
    };
    let end_labels = match kind {
        "hotel" => "check[ -]?out|departure|end",
        "car" => "drop[ -]?off|return|end",
        _ => "arrival|arrive|end",
    };
    (
        labelled_value(text, start_labels).and_then(|v| normalize_datetime(&v)),
        labelled_value(text, end_labels).and_then(|v| normalize_datetime(&v)),
    )
}

fn normalize_datetime(raw: &str) -> Option<String> {
    let cleaned = raw
        .trim()
        .trim_matches(|c: char| c == ',' || c == '.' || c == '-')
        .replace(" at ", " ");
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&cleaned) {
        return Some(dt.to_rfc3339());
    }
    let date_times = [
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d %I:%M %p",
        "%b %d, %Y %I:%M %p",
        "%B %d, %Y %I:%M %p",
        "%a, %b %d, %Y %I:%M %p",
        "%A, %B %d, %Y %I:%M %p",
        "%d %b %Y %H:%M",
        "%d %B %Y %H:%M",
    ];
    for fmt in date_times {
        if let Ok(dt) = NaiveDateTime::parse_from_str(&cleaned, fmt) {
            return Some(dt.format("%Y-%m-%dT%H:%M:%S").to_string());
        }
    }
    for fmt in ["%Y-%m-%d", "%b %d, %Y", "%B %d, %Y", "%d %b %Y", "%d %B %Y"] {
        if let Ok(d) = NaiveDate::parse_from_str(&cleaned, fmt) {
            return Some(format!("{}T00:00:00", d.format("%Y-%m-%d")));
        }
    }
    None
}

fn ambiguous_numeric_date(text: &str) -> bool {
    static AMBIGUOUS: OnceLock<Regex> = OnceLock::new();
    re(
        r"\b(?:0?[1-9]|1[0-2])/(?:0?[1-9]|1[0-2])/(?:20)?\d{2}\b",
        &AMBIGUOUS,
    )
    .is_match(text)
}

fn infer_status(subject: &str, text: &str, confirmation: Option<&str>) -> String {
    static CANCEL_DIRECT: OnceLock<Regex> = OnceLock::new();
    static CHANGE: OnceLock<Regex> = OnceLock::new();
    let combined = format!("{subject}\n{text}");
    let direct = re(
        r"(?i)\b(your (?:booking|reservation|flight|trip) (?:has been |was )?(?:cancelled|canceled)|(?:booking|reservation|flight) cancelled|cancellation confirmed)\b",
        &CANCEL_DIRECT,
    )
    .is_match(&combined);
    if direct && confirmation.is_some() {
        "cancelled".into()
    } else if re(
        r"(?i)\b(schedule change|updated itinerary|new departure time|booking changed)\b",
        &CHANGE,
    )
    .is_match(&combined)
    {
        "changed".into()
    } else {
        "confirmed".into()
    }
}

fn normalize_status(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("cancel") {
        "cancelled".into()
    } else if lower.contains("change") || lower.contains("modif") || lower.contains("update") {
        "changed".into()
    } else if lower.contains("pending") {
        "pending".into()
    } else if lower.contains("confirm") {
        "confirmed".into()
    } else {
        "unknown".into()
    }
}

fn infer_title(subject: &str, parsed: &ParsedTravelDocument) -> String {
    if !parsed.title.trim().is_empty() && parsed.title != clean_subject(subject) {
        return parsed.title.clone();
    }
    match parsed.kind.as_str() {
        "flight" => match (&parsed.origin, &parsed.destination) {
            (Some(from), Some(to)) => format!("Flight {from} to {to}"),
            _ => clean_subject(subject),
        },
        "hotel" => parsed
            .destination
            .clone()
            .unwrap_or_else(|| clean_subject(subject)),
        _ => clean_subject(subject),
    }
}

fn clean_subject(subject: &str) -> String {
    static PREFIX: OnceLock<Regex> = OnceLock::new();
    re(r"(?i)^\s*(?:(?:fwd?|re)\s*:\s*)+", &PREFIX)
        .replace(subject.trim(), "")
        .trim()
        .to_string()
}

fn provider_from_sender(from: &str) -> Option<String> {
    let trimmed = from.trim();
    if let Some((name, _)) = trimmed.split_once('<') {
        let name = name.trim().trim_matches('"');
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    let email = trimmed.trim_matches(['<', '>']);
    let domain = email.split('@').nth(1)?.split('>').next()?.trim();
    if domain.ends_with(".local") || domain.ends_with(".invalid") {
        return None;
    }
    let provider = domain
        .split('.')
        .next()
        .unwrap_or(domain)
        .replace(['-', '_'], " ");
    (!provider.is_empty()).then(|| title_case(&provider))
}

fn title_case(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn extract_amount(text: &str) -> (Option<i64>, Option<String>) {
    static AMOUNT: OnceLock<Regex> = OnceLock::new();
    let caps = re(
        r"(?i)(?:\b(USD|EUR|GBP|CAD|AUD)\s*|([$€£])\s*)(\d{1,6}(?:[,.]\d{3})*(?:[,.]\d{2})?)\b",
        &AMOUNT,
    )
    .captures(text);
    let Some(caps) = caps else {
        return (None, None);
    };
    let currency = caps.get(1).map(|m| m.as_str().to_uppercase()).or_else(|| {
        match caps.get(2).map(|m| m.as_str()) {
            Some("$") => Some("USD".into()),
            Some("€") => Some("EUR".into()),
            Some("£") => Some("GBP".into()),
            _ => None,
        }
    });
    let amount = caps.get(3).and_then(|m| parse_money_minor(m.as_str()));
    (amount, currency)
}

fn parse_money_minor(raw: &str) -> Option<i64> {
    let mut normalized = raw.trim().replace(' ', "");
    match (normalized.rfind(','), normalized.rfind('.')) {
        (Some(comma), Some(dot)) if comma > dot => {
            normalized = normalized.replace('.', "").replace(',', ".");
        }
        (Some(_), None) => normalized = normalized.replace(',', "."),
        _ => normalized = normalized.replace(',', ""),
    }
    let amount: f64 = normalized.parse().ok()?;
    Some((amount * 100.0).round() as i64)
}

fn required_fields_present(parsed: &ParsedTravelDocument) -> bool {
    match parsed.kind.as_str() {
        "flight" | "rail" => {
            parsed.start_at.is_some()
                && parsed.confirmation_code.is_some()
                && (parsed.service_number.is_some()
                    || (parsed.origin.is_some() && parsed.destination.is_some()))
        }
        "hotel" => {
            parsed.start_at.is_some()
                && parsed.end_at.is_some()
                && parsed.confirmation_code.is_some()
        }
        "car" => parsed.start_at.is_some() && parsed.confirmation_code.is_some(),
        "activity" => parsed.start_at.is_some(),
        "receipt" => parsed.amount_minor.is_some(),
        _ => false,
    }
}

fn strip_html(html: &str) -> String {
    static TAGS: OnceLock<Regex> = OnceLock::new();
    static WS: OnceLock<Regex> = OnceLock::new();
    let without_tags = re(r"(?is)<[^>]+>", &TAGS).replace_all(html, " ");
    re(r"[ \t\r\n]+", &WS)
        .replace_all(&without_tags, " ")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .trim()
        .to_string()
}

fn first_address(header: Option<&mail_parser::Address<'_>>) -> String {
    let item = match header {
        Some(mail_parser::Address::List(list)) => list.first(),
        Some(mail_parser::Address::Group(groups)) => {
            groups.first().and_then(|group| group.addresses.first())
        }
        None => None,
    };
    let Some(item) = item else {
        return String::new();
    };
    match (item.name.as_deref(), item.address.as_deref()) {
        (Some(name), Some(address)) if !name.trim().is_empty() => {
            format!("{} <{}>", name.trim(), address.trim())
        }
        (_, Some(address)) => address.trim().to_string(),
        (Some(name), None) => name.trim().to_string(),
        _ => String::new(),
    }
}

fn first_address_domain(header: Option<&mail_parser::Address<'_>>) -> Option<String> {
    let item = match header {
        Some(mail_parser::Address::List(list)) => list.first(),
        Some(mail_parser::Address::Group(groups)) => {
            groups.first().and_then(|group| group.addresses.first())
        }
        None => None,
    }?;
    let address = item.address.as_deref()?.trim();
    let (_, domain) = address.rsplit_once('@')?;
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    (!domain.is_empty()
        && domain.len() <= 253
        && domain
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-')))
    .then_some(domain)
}

fn truncate_chars(mut value: String, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value;
    }
    let byte_end = value
        .char_indices()
        .nth(max_chars)
        .map(|(index, _)| index)
        .unwrap_or(value.len());
    value.truncate(byte_end);
    value.push_str("\n[truncated by Envelope Travel]");
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_ld_flight_takes_structured_path() {
        let html = r#"
          <script type="application/ld+json">
          {"@type":"FlightReservation","reservationNumber":"ABC123",
           "reservationFor":{"@type":"Flight","flightNumber":"AF008",
             "airline":{"name":"Air France"},
             "departureAirport":{"iataCode":"JFK"},"arrivalAirport":{"iataCode":"CDG"},
             "departureTime":"2026-09-12T19:30:00-04:00","arrivalTime":"2026-09-13T08:45:00+02:00"}}
          </script>"#;
        let parsed = parse_travel_document(
            "Air France <confirmation@airfrance.com>",
            "Your trip is confirmed",
            None,
            "",
            Some(html),
            None,
        );
        assert_eq!(parsed.kind, "flight");
        assert_eq!(parsed.confirmation_code.as_deref(), Some("ABC123"));
        assert_eq!(parsed.service_number.as_deref(), Some("AF008"));
        assert_eq!(parsed.origin.as_deref(), Some("JFK"));
        assert_eq!(parsed.destination.as_deref(), Some("CDG"));
        assert_eq!(parsed.decision, "auto_accepted");
    }

    #[test]
    fn generic_hotel_receipt_extracts_label_fields() {
        let body = "Reservation confirmed\nConfirmation: HTL-93822\nHotel: Casa Bonay Barcelona\nCheck-in: September 12, 2026 3:00 PM\nCheck-out: September 15, 2026 11:00 AM\nTotal: EUR 624.10";
        let parsed = parse_travel_document(
            "Booking.com <noreply@booking.com>",
            "Hotel reservation confirmed",
            None,
            body,
            None,
            None,
        );
        assert_eq!(parsed.kind, "hotel");
        assert_eq!(parsed.confirmation_code.as_deref(), Some("HTL-93822"));
        assert_eq!(parsed.destination.as_deref(), Some("Casa Bonay Barcelona"));
        assert_eq!(parsed.amount_minor, Some(62_410));
        assert_eq!(parsed.currency.as_deref(), Some("EUR"));
        assert!(
            parsed
                .start_at
                .as_deref()
                .unwrap()
                .starts_with("2026-09-12")
        );
    }

    #[test]
    fn free_cancellation_policy_does_not_cancel_booking() {
        let body = "Confirmation: HOTEL55\nHotel: Example House\nCheck-in: Sep 12, 2026 3:00 PM\nCheck-out: Sep 15, 2026 11:00 AM\nFree cancellation until Sep 10.";
        let parsed = parse_travel_document(
            "Hotel <stay@example.test>",
            "Your hotel reservation is confirmed",
            None,
            body,
            None,
            None,
        );
        assert_eq!(parsed.status, "confirmed");
    }

    #[test]
    fn explicit_cancellation_requires_labelled_identity() {
        let body = "Your flight was cancelled. Record locator: CAN123";
        let parsed = parse_travel_document(
            "United <updates@united.com>",
            "Your flight was cancelled",
            None,
            body,
            None,
            None,
        );
        assert_eq!(parsed.status, "cancelled");

        let no_identity = parse_travel_document(
            "United <updates@united.com>",
            "Cancellation information",
            None,
            "See our cancellation policy.",
            None,
            None,
        );
        assert_ne!(no_identity.status, "cancelled");
    }

    #[test]
    fn actual_cancellation_wins_even_when_the_footer_mentions_policy() {
        let parsed = parse_travel_document(
            "United <updates@united.com>",
            "Your flight was cancelled",
            None,
            "Your flight was cancelled. Record locator: CAN123\nCancellation policy: refunds take five days.",
            None,
            None,
        );
        assert_eq!(parsed.status, "cancelled");
    }

    #[test]
    fn structured_cancellation_status_is_preserved() {
        let html = r#"<script type="application/ld+json">
          {"@type":"FlightReservation","reservationNumber":"CAN123",
           "reservationStatus":"https://schema.org/ReservationCancelled",
           "reservationFor":{"@type":"Flight","flightNumber":"UA100",
             "departureAirport":{"iataCode":"SFO"},"arrivalAirport":{"iataCode":"JFK"},
             "departureTime":"2026-09-12T10:00:00-07:00"}}
          </script>"#;
        let parsed = parse_travel_document(
            "United <updates@united.com>",
            "Itinerary notice",
            None,
            "Record locator: CAN123",
            Some(html),
            None,
        );
        assert_eq!(parsed.status, "cancelled");
    }

    #[test]
    fn message_received_time_is_never_invented_as_departure_time() {
        let parsed = parse_travel_document(
            "Air France <confirmation@airfrance.com>",
            "Flight confirmation",
            Some("2026-08-31T09:00:00Z"),
            "Record locator: ABC123\nFlight: AF008\nDestination: CDG",
            None,
            None,
        );
        assert_eq!(parsed.start_at, None);
        assert_ne!(parsed.decision, "auto_accepted");
    }

    #[test]
    fn generic_reservation_subject_is_reviewed_not_dropped() {
        let decision = classify_candidate(
            "Independent Inn <hello@independent.example>",
            "Your reservation is confirmed",
            "",
        );
        assert_eq!(decision.state, "review");
    }

    #[test]
    fn marketing_fare_sale_is_ignored() {
        let decision = classify_candidate(
            "Deals <offers@airline.example>",
            "Flash fare sale to Paris",
            "Newsletter. Unsubscribe. Travel inspiration and deal alert.",
        );
        assert_eq!(decision.state, "ignored");
    }

    #[test]
    fn ambiguous_numeric_date_is_review_only() {
        let parsed = parse_travel_document(
            "Unknown Travel <hello@unknown.test>",
            "Booking confirmation",
            None,
            "Confirmation: ABC999\nHotel: The Example\nCheck-in: 03/04/2026\nCheck-out: 04/04/2026",
            None,
            None,
        );
        assert!(
            parsed
                .reasons
                .contains(&"ambiguous_numeric_date".to_string())
        );
        assert_ne!(parsed.decision, "auto_accepted");
    }

    #[test]
    fn ics_event_is_a_timeline_activity() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:voucher-77\r\nDTSTART:20260914T090000Z\r\nDTEND:20260914T110000Z\r\nSUMMARY:Sagrada Familia tour\r\nLOCATION:Carrer de Mallorca 401\r\nEND:VEVENT\r\nEND:VCALENDAR";
        let parsed = parse_travel_document(
            "Tours <tickets@tour.example>",
            "Your activity voucher",
            None,
            "Voucher attached",
            None,
            Some(ics),
        );
        assert_eq!(parsed.kind, "activity");
        assert_eq!(parsed.title, "Sagrada Familia tour");
        assert_eq!(parsed.start_at.as_deref(), Some("2026-09-14T09:00:00Z"));
    }

    #[test]
    fn raw_email_decoder_keeps_calendar_and_attachment_provenance() {
        let raw = concat!(
            "From: Air France <confirmation@airfrance.example>\r\n",
            "To: family@example.test\r\n",
            "Subject: Flight confirmation\r\n",
            "Date: Mon, 31 Aug 2026 09:00:00 +0000\r\n",
            "Message-ID: <fixture-1@example.test>\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=fixture\r\n\r\n",
            "--fixture\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n",
            "Record locator: ABC123\r\n",
            "--fixture\r\nContent-Type: text/calendar\r\n",
            "Content-Disposition: attachment; filename=trip.ics\r\n\r\n",
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:fixture\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            "--fixture--\r\n"
        );
        let decoded = parse_raw_travel_email(raw.as_bytes()).unwrap();
        assert_eq!(
            decoded.message_id.as_deref(),
            Some("fixture-1@example.test")
        );
        assert_eq!(
            decoded.from_addr,
            "Air France <confirmation@airfrance.example>"
        );
        assert!(decoded.text_body.contains("ABC123"));
        assert_eq!(decoded.attachment_names, vec!["trip.ics"]);
        assert!(
            decoded
                .ics_body
                .as_deref()
                .unwrap()
                .contains("BEGIN:VEVENT")
        );
        assert_eq!(decoded.authenticated_sender_domain, None);
    }

    #[test]
    fn gmail_dmarc_alignment_authenticates_the_mailbox_domain_not_display_name() {
        let raw = concat!(
            "Authentication-Results: mx.google.com;\r\n",
            " dkim=pass header.i=@airfrance.example;\r\n",
            " spf=pass smtp.mailfrom=airfrance.example;\r\n",
            " dmarc=pass (p=REJECT) header.from=airfrance.example\r\n",
            "From: Untrusted Display Name <tickets@airfrance.example>\r\n",
            "To: family@example.test\r\n",
            "Subject: Reservation changed\r\n",
            "Date: Mon, 31 Aug 2026 09:00:00 +0000\r\n",
            "Message-ID: <auth-fixture@example.test>\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n\r\n",
            "PNR: ABC123\r\n"
        );
        let decoded = parse_raw_travel_email(raw.as_bytes()).unwrap();
        assert_eq!(
            decoded.authenticated_sender_domain.as_deref(),
            Some("airfrance.example")
        );
    }

    #[test]
    fn spoofed_authentication_result_or_unaligned_from_domain_never_authenticates() {
        let forged_authserv = concat!(
            "Authentication-Results: attacker.example; dmarc=pass header.from=airfrance.example\r\n",
            "From: Air France <tickets@airfrance.example>\r\n",
            "Subject: Reservation cancelled\r\n",
            "Date: Mon, 31 Aug 2026 09:00:00 +0000\r\n\r\n",
            "PNR: ABC123\r\n"
        );
        assert_eq!(
            parse_raw_travel_email(forged_authserv.as_bytes())
                .unwrap()
                .authenticated_sender_domain,
            None
        );

        let unaligned = concat!(
            "Authentication-Results: mx.google.com; dmarc=pass header.from=airfrance.example\r\n",
            "From: Air France <attacker@evil.example>\r\n",
            "Subject: Reservation cancelled\r\n",
            "Date: Mon, 31 Aug 2026 09:00:00 +0000\r\n\r\n",
            "PNR: ABC123\r\n"
        );
        assert_eq!(
            parse_raw_travel_email(unaligned.as_bytes())
                .unwrap()
                .authenticated_sender_domain,
            None
        );
    }

    fn parse_fixture(raw: &[u8]) -> ParsedTravelDocument {
        let decoded = parse_raw_travel_email(raw).expect("fixture must be valid RFC822");
        parse_travel_document(
            &decoded.from_addr,
            &decoded.subject,
            decoded.received_at.as_deref(),
            &decoded.text_body,
            decoded.html_body.as_deref(),
            decoded.ics_body.as_deref(),
        )
    }

    #[test]
    fn redacted_rfc822_fixture_corpus_is_a_regression_gate() {
        let flight = parse_fixture(include_bytes!("../tests/fixtures/travel/flight-jsonld.eml"));
        assert_eq!(flight.kind, "flight");
        assert_eq!(flight.confirmation_code.as_deref(), Some("ABC123"));
        assert_eq!(flight.decision, "auto_accepted");

        let hotel = parse_fixture(include_bytes!("../tests/fixtures/travel/hotel-text.eml"));
        assert_eq!(hotel.kind, "hotel");
        assert_eq!(hotel.confirmation_code.as_deref(), Some("HTL-93822"));
        assert_ne!(hotel.decision, "ignored");

        let marketing = parse_fixture(include_bytes!("../tests/fixtures/travel/marketing.eml"));
        assert_eq!(marketing.decision, "ignored");
    }
}
