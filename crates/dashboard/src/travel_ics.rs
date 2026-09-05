// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Minimal RFC 5545 renderer for tokenized, redacted travel feeds.
//!
//! The feed receives an already-redacted event model. Confirmation codes,
//! passenger addresses, receipt text, and private notes are intentionally not
//! representable here, which keeps later callers from leaking them by accident.

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TravelCalendarEvent {
    pub id: String,
    pub summary: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_at: String,
    pub end_at: Option<String>,
    pub timezone: Option<String>,
    pub status: String,
    pub sequence: u32,
    pub updated_at: Option<String>,
    pub url: Option<String>,
    pub alarm_minutes: Option<u32>,
}

pub fn render_travel_calendar(name: &str, events: &[TravelCalendarEvent]) -> String {
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        "PRODID:-//Envelope//Travel//EN".to_string(),
        "CALSCALE:GREGORIAN".to_string(),
        "METHOD:PUBLISH".to_string(),
        format!("X-WR-CALNAME:{}", escape_text(name)),
        "X-PUBLISHED-TTL:PT15M".to_string(),
    ];

    for event in events {
        if DateTime::parse_from_rfc3339(&event.start_at).is_err()
            && NaiveDate::parse_from_str(&event.start_at, "%Y-%m-%d").is_err()
            && NaiveDateTime::parse_from_str(&event.start_at, "%Y-%m-%dT%H:%M:%S").is_err()
        {
            continue;
        }
        lines.push("BEGIN:VEVENT".into());
        lines.push(format!(
            "UID:segment-{}@envelope.travel",
            safe_uid(&event.id)
        ));
        lines.push(format!(
            "DTSTAMP:{}",
            utc_stamp(event.updated_at.as_deref())
        ));
        if let Some(updated) = event.updated_at.as_deref() {
            lines.push(format!("LAST-MODIFIED:{}", utc_stamp(Some(updated))));
        }
        lines.push(format!("SEQUENCE:{}", event.sequence));
        lines.push(format!("SUMMARY:{}", escape_text(&event.summary)));
        if let Some(description) = event
            .description
            .as_deref()
            .filter(|v| !v.trim().is_empty())
        {
            lines.push(format!("DESCRIPTION:{}", escape_text(description)));
        }
        if let Some(location) = event.location.as_deref().filter(|v| !v.trim().is_empty()) {
            lines.push(format!("LOCATION:{}", escape_text(location)));
        }
        lines.push(render_date(
            "DTSTART",
            &event.start_at,
            event.timezone.as_deref(),
        ));
        if let Some(end) = event.end_at.as_deref().filter(|v| !v.trim().is_empty()) {
            lines.push(render_date("DTEND", end, event.timezone.as_deref()));
        }
        if event.status.eq_ignore_ascii_case("cancelled")
            || event.status.eq_ignore_ascii_case("canceled")
        {
            lines.push("STATUS:CANCELLED".into());
        } else {
            lines.push("STATUS:CONFIRMED".into());
        }
        if let Some(url) = event.url.as_deref().filter(|url| valid_action_url(url)) {
            lines.push(format!("URL:{url}"));
        }
        if !event.status.eq_ignore_ascii_case("cancelled")
            && !event.status.eq_ignore_ascii_case("canceled")
        {
            if let Some(minutes) = event.alarm_minutes.filter(|minutes| *minutes <= 10080) {
                lines.extend([
                    "BEGIN:VALARM".into(),
                    "ACTION:DISPLAY".into(),
                    format!("TRIGGER:-PT{minutes}M"),
                    format!("DESCRIPTION:{}", escape_text(&event.summary)),
                    "END:VALARM".into(),
                ]);
            }
        }
        lines.push("TRANSP:OPAQUE".into());
        lines.push("END:VEVENT".into());
    }
    lines.push("END:VCALENDAR".into());

    lines
        .into_iter()
        .flat_map(|line| fold_line(&line))
        .collect::<Vec<_>>()
        .join("\r\n")
        + "\r\n"
}

pub fn valid_action_url(raw: &str) -> bool {
    !raw.chars().any(char::is_control)
        && url::Url::parse(raw).is_ok_and(|url| {
            matches!(url.scheme(), "https" | "http")
                && url.username().is_empty()
                && url.password().is_none()
        })
}

fn render_date(field: &str, raw: &str, timezone: Option<&str>) -> String {
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return format!(
            "{field}:{}",
            dt.with_timezone(&Utc).format("%Y%m%dT%H%M%SZ")
        );
    }
    if let Ok(date) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return format!("{field};VALUE=DATE:{}", date.format("%Y%m%d"));
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S") {
        if let Some(tzid) = timezone.filter(|v| valid_tzid(v)) {
            return format!("{field};TZID={tzid}:{}", dt.format("%Y%m%dT%H%M%S"));
        }
        return format!("{field}:{}", dt.format("%Y%m%dT%H%M%S"));
    }
    // Invalid stored input must remain inert text, never become a malformed
    // property injection. A midnight all-day placeholder preserves the event
    // while making the review need visible in the dashboard.
    format!("{field};VALUE=DATE:19700101")
}

fn valid_tzid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'+'))
}

fn utc_stamp(raw: Option<&str>) -> String {
    raw.and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|dt| dt.with_timezone(&Utc).format("%Y%m%dT%H%M%SZ").to_string())
        .unwrap_or_else(|| Utc::now().format("%Y%m%dT%H%M%SZ").to_string())
}

fn escape_text(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace(['\r', '\n'], "\\n")
        .replace(';', "\\;")
        .replace(',', "\\,")
}

fn safe_uid(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_') {
                (b as char).to_string()
            } else {
                format!("-{b:02x}")
            }
        })
        .collect()
}

/// Fold content lines at 75 UTF-8 octets. Continuation lines start with one
/// space, as RFC 5545 requires. Splits only at character boundaries.
fn fold_line(line: &str) -> Vec<String> {
    if line.len() <= 75 {
        return vec![line.to_string()];
    }
    let mut remaining = line;
    let mut out = Vec::new();
    let mut first = true;
    while !remaining.is_empty() {
        let budget = if first { 75 } else { 74 };
        let mut end = remaining.len().min(budget);
        while !remaining.is_char_boundary(end) {
            end -= 1;
        }
        let chunk = &remaining[..end];
        out.push(if first {
            chunk.to_string()
        } else {
            format!(" {chunk}")
        });
        remaining = &remaining[end..];
        first = false;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> TravelCalendarEvent {
        TravelCalendarEvent {
            id: "seg/42".into(),
            summary: "Flight Paris, then Madrid".into(),
            description: Some("Board at terminal 2; family meets outside.\nBring IDs.".into()),
            location: Some("Charles de Gaulle; T2".into()),
            start_at: "2026-09-12T19:30:00+02:00".into(),
            end_at: Some("2026-09-12T21:45:00+02:00".into()),
            timezone: Some("Europe/Paris".into()),
            status: "confirmed".into(),
            sequence: 2,
            updated_at: Some("2026-08-31T09:00:00Z".into()),
            url: Some("https://example.test/manage".into()),
            alarm_minutes: Some(120),
        }
    }

    #[test]
    fn calendar_has_stable_uid_dates_sequence_and_escaped_text() {
        let rendered = render_travel_calendar("Family travel", &[event()]);
        assert!(rendered.contains("UID:segment-seg-2f42@envelope.travel\r\n"));
        assert!(rendered.contains("SEQUENCE:2\r\n"));
        assert!(rendered.contains("DTSTART:20260912T173000Z\r\n"));
        assert!(rendered.contains("SUMMARY:Flight Paris\\, then Madrid\r\n"));
        assert!(rendered.contains("LOCATION:Charles de Gaulle\\; T2\r\n"));
        assert!(rendered.ends_with("END:VCALENDAR\r\n"));
    }

    #[test]
    fn floating_local_time_uses_valid_tzid() {
        let mut item = event();
        item.start_at = "2026-09-12T19:30:00".into();
        let rendered = render_travel_calendar("Trip", &[item]);
        assert!(rendered.contains("DTSTART;TZID=Europe/Paris:20260912T193000\r\n"));
    }

    #[test]
    fn cancelled_booking_stays_in_feed_as_cancelled() {
        let mut item = event();
        item.status = "cancelled".into();
        assert!(render_travel_calendar("Trip", &[item]).contains("STATUS:CANCELLED\r\n"));
    }

    #[test]
    fn feed_model_cannot_carry_confirmation_or_receipt_secrets() {
        let rendered = render_travel_calendar("Trip", &[event()]);
        assert!(!rendered.contains("ABC123"));
        assert!(!rendered.to_lowercase().contains("confirmation"));
        assert!(!rendered.to_lowercase().contains("receipt body"));
    }

    #[test]
    fn long_unicode_lines_fold_under_octet_limit() {
        let mut item = event();
        item.summary = "Très long family itinerary — ".repeat(8);
        let rendered = render_travel_calendar("Trip", &[item]);
        for line in rendered.split("\r\n") {
            assert!(line.len() <= 75, "line is {} bytes: {line:?}", line.len());
        }
        assert!(rendered.lines().any(|line| line.starts_with(' ')));
    }
}
