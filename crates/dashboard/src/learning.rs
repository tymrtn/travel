//! Constrained, versioned receipt parsers. Generated rules never execute code.
use crate::travel_parser::ParsedTravelDocument;
use anyhow::{Result, ensure};
use envelope_email_store::Database;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldRule {
    pub field: String,
    /// One capture group extracts a literal source span. No model constants.
    pub pattern: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParserPackage {
    pub version: u32,
    pub name: String,
    pub sender: String,
    pub subject_pattern: String,
    pub kind: String,
    pub fields: Vec<FieldRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fixture {
    pub sender: String,
    pub subject: String,
    pub body: String,
    pub expected: Value,
}

pub fn initialize(db: &Database) -> Result<()> {
    db.conn().execute_batch(
        "CREATE TABLE IF NOT EXISTS learned_parsers (
      id TEXT PRIMARY KEY, name TEXT NOT NULL, package TEXT NOT NULL,
      fixtures TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 0,
      created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
      CREATE UNIQUE INDEX IF NOT EXISTS learned_active_name ON learned_parsers(name) WHERE active=1;
      CREATE TABLE IF NOT EXISTS learning_jobs (
      id TEXT PRIMARY KEY, receipt_id TEXT NOT NULL, state TEXT NOT NULL,
      provider TEXT, failure TEXT, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);",
    )?;
    Ok(())
}

impl ParserPackage {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "Unsupported parser version");
        ensure!(
            !self.name.is_empty() && self.name.len() <= 100,
            "Invalid parser name"
        );
        ensure!(
            self.sender.contains('@') && !self.sender.contains(['\r', '\n']),
            "Exact sender required"
        );
        ensure!(
            ["flight", "hotel", "train", "car", "activity"].contains(&self.kind.as_str()),
            "Unsupported booking kind"
        );
        ensure!(
            !self.subject_pattern.is_empty() && self.subject_pattern.len() <= 1024,
            "Subject matcher required"
        );
        Regex::new(&self.subject_pattern)?;
        ensure!(self.fields.len() <= 20, "Too many fields");
        let allowed = [
            "title",
            "provider",
            "confirmation_code",
            "start_at",
            "end_at",
            "timezone",
            "origin",
            "destination",
            "address",
            "service_number",
            "currency",
        ];
        let mut seen = std::collections::HashSet::new();
        for rule in &self.fields {
            ensure!(
                allowed.contains(&rule.field.as_str()) && seen.insert(&rule.field),
                "Unsupported or duplicate field"
            );
            ensure!(rule.pattern.len() <= 4096, "Pattern too large");
            ensure!(
                Regex::new(&rule.pattern)?.captures_len() == 2,
                "Each rule must extract exactly one source span"
            );
        }
        ensure!(
            seen.contains(&"start_at".to_string()) && seen.contains(&"title".to_string()),
            "Title and date evidence required"
        );
        Ok(())
    }

    pub fn extract(&self, sender: &str, subject: &str, body: &str) -> Result<Value> {
        self.validate()?;
        ensure!(body.len() <= 1024 * 1024, "Receipt exceeds parser limit");
        ensure!(
            sender.trim().eq_ignore_ascii_case(&self.sender)
                && Regex::new(&self.subject_pattern)?.is_match(subject),
            "Parser does not match"
        );
        let mut fields = serde_json::Map::new();
        fields.insert("kind".into(), json!(self.kind));
        // Learned parsers may extract confirmations, but never authorize an amendment.
        fields.insert("status".into(), json!("confirmed"));
        for rule in &self.fields {
            let expression = Regex::new(&rule.pattern)?;
            let matches: Vec<_> = expression.captures_iter(body).collect();
            ensure!(
                matches.len() == 1,
                "Field {} missing or ambiguous",
                rule.field
            );
            let value = matches[0].get(1).ok_or_else(||anyhow::anyhow!("Field {} has no captured evidence",rule.field))?.as_str().trim();
            ensure!(
                !value.is_empty() && value.len() <= 2048,
                "Invalid field evidence"
            );
            fields.insert(rule.field.clone(), json!(value));
        }
        let start = chrono::DateTime::parse_from_rfc3339(fields["start_at"].as_str().unwrap())?;
        if let Some(end) = fields.get("end_at").and_then(Value::as_str) {
            ensure!(
                chrono::DateTime::parse_from_rfc3339(end)? >= start,
                "Arrival precedes departure"
            );
        }
        Ok(Value::Object(fields))
    }
}

/// The expected fields must all be independently extracted verbatim. Replay old
/// fixtures as well, then atomically swap only this parser family.
pub fn promote(db: &mut Database, package: &ParserPackage, fixtures: &[Fixture]) -> Result<String> {
    initialize(db)?;
    ensure!(
        !fixtures.is_empty() && fixtures.len() <= 100,
        "Fixtures required"
    );
    for fixture in fixtures {
        let result = package.extract(&fixture.sender, &fixture.subject, &fixture.body)?;
        ensure!(result == fixture.expected, "Fixture replay mismatch");
        // Negative mutation: removing each captured source value must fail or
        // change that value. A parser that returns constants cannot pass.
        for rule in &package.fields {
            let value = result[&rule.field].as_str().unwrap();
            let mutated = fixture.body.replace(value, "");
            if let Ok(other) = package.extract(&fixture.sender, &fixture.subject, &mutated) {
                ensure!(
                    other[&rule.field] != result[&rule.field],
                    "Mutation test failed"
                );
            }
        }
    }
    let mut all = fixtures.to_vec();
    {
        let mut stmt = db
            .conn()
            .prepare("SELECT fixtures FROM learned_parsers WHERE name=?1 AND active=1")?;
        for row in stmt.query_map([&package.name], |row| row.get::<_, String>(0))? {
            for fixture in serde_json::from_str::<Vec<Fixture>>(&row?)? {
                ensure!(
                    package.extract(&fixture.sender, &fixture.subject, &fixture.body)?
                        == fixture.expected,
                    "Historical regression"
                );
                all.push(fixture);
            }
        }
    }
    let encoded = serde_json::to_string(package)?;
    let id = format!("{:x}", Sha256::digest(encoded.as_bytes()));
    let tx = db.conn_mut().transaction()?;
    tx.execute(
        "UPDATE learned_parsers SET active=0 WHERE name=?1",
        [&package.name],
    )?;
    tx.execute(
        "INSERT INTO learned_parsers(id,name,package,fixtures,active) VALUES(?1,?2,?3,?4,1)
      ON CONFLICT(id) DO UPDATE SET active=1,fixtures=excluded.fixtures",
        rusqlite::params![id, package.name, encoded, serde_json::to_string(&all)?],
    )?;
    tx.commit()?;
    Ok(id)
}

pub fn known(
    db: &Database,
    sender: &str,
    subject: &str,
    body: &str,
) -> Result<Option<ParsedTravelDocument>> {
    initialize(db)?;
    let mut stmt = db.conn().prepare(
        "SELECT id,package,fixtures FROM learned_parsers WHERE active=1 ORDER BY created_at DESC",
    )?;
    let mut candidates = Vec::new();
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (id, encoded, fixtures) = row?;
        let package: ParserPackage = serde_json::from_str(&encoded)?;
        let fixtures: Vec<Fixture> = serde_json::from_str(&fixtures)?;
        // One fixture cannot establish broad coverage. Only the demonstrated
        // literal layout, after substituting captured fields, may use the rule.
        let Some(layout) = evidence_layout(&package, body) else {
            continue;
        };
        if !fixtures.iter().any(|f| {
            f.subject == subject && evidence_layout(&package, &f.body).as_ref() == Some(&layout)
        }) {
            continue;
        }
        if let Ok(mut value) = package.extract(sender, subject, body) {
            let object = value.as_object_mut().unwrap();
            object.insert("confidence".into(), json!(0.9));
            object.insert("decision".into(), json!("quarantined"));
            object.insert(
                "reasons".into(),
                json!(["Learned extraction requires booking identity review"]),
            );
            object.insert("parser_version".into(), json!(format!("learned:{id}")));
            candidates.push(serde_json::from_value(value)?);
        }
    }
    // Competing matching formats are never resolved by arbitrary ordering.
    Ok(if candidates.len() == 1 {
        candidates.pop()
    } else {
        None
    })
}

fn evidence_layout(package: &ParserPackage, body: &str) -> Option<String> {
    let mut spans = Vec::new();
    for rule in &package.fields {
        let expression = Regex::new(&rule.pattern).ok()?;
        let mut matches = expression.captures_iter(body);
        let capture = matches.next()?.get(1)?;
        if matches.next().is_some() {
            return None;
        }
        spans.push((capture.start(), capture.end(), rule.field.as_str()));
    }
    spans.sort_by_key(|span| span.0);
    if spans.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return None;
    }
    let mut layout = body.to_owned();
    for (start, end, field) in spans.into_iter().rev() {
        layout.replace_range(start..end, &format!("{{{field}}}"));
    }
    Some(layout)
}

pub fn rollback(db: &mut Database, id: &str) -> Result<()> {
    initialize(db)?;
    let (name, encoded, fixtures): (String, String, String) = db.conn().query_row(
        "SELECT name,package,fixtures FROM learned_parsers WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let package: ParserPackage = serde_json::from_str(&encoded)?;
    package.validate()?;
    for fixture in serde_json::from_str::<Vec<Fixture>>(&fixtures)? {
        ensure!(
            package.extract(&fixture.sender, &fixture.subject, &fixture.body)? == fixture.expected,
            "Stored parser no longer validates"
        );
    }
    let tx = db
        .conn_mut()
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute("UPDATE learned_parsers SET active=0 WHERE name=?1", [name])?;
    tx.execute("UPDATE learned_parsers SET active=1 WHERE id=?1", [id])?;
    tx.commit()?;
    Ok(())
}

pub async fn versions(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let result:Result<Value>=(|| async {
        let db=state.db.lock().await;initialize(&db)?;
        let mut stmt=db.conn().prepare("SELECT id,name,active,created_at FROM learned_parsers ORDER BY created_at DESC")?;
        let rows=stmt.query_map([],|r|Ok(json!({"id":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?,"active":r.get::<_,bool>(2)?,"created_at":r.get::<_,String>(3)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
        Ok(json!({"version":1,"parsers":rows}))
    })().await;
    match result {
        Ok(value) => axum::Json(value).into_response(),
        Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
pub async fn restore_version(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut db = state.db.lock().await;
    match rollback(&mut db, &id) {
        Ok(()) => axum::Json(json!({"status":"restored"})).into_response(),
        Err(_) => (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error":"parser_not_found_or_invalid"})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn optional_capture_abstains_instead_of_panicking() {
        let mut p=package();
        p.fields[0].pattern="Title: ([^\\n]+)|Confirmed".into();
        assert!(p.extract(&p.sender,"Confirmed","Confirmed\nStart: 2026-10-10T10:00:00Z").is_err());
    }
    fn package() -> ParserPackage {
        ParserPackage {
            version: 1,
            name: "fixture-air".into(),
            sender: "tickets@example.test".into(),
            subject_pattern: "^Confirmed$".into(),
            kind: "flight".into(),
            fields: vec![
                FieldRule {
                    field: "title".into(),
                    pattern: "Title: ([^\\n]+)".into(),
                },
                FieldRule {
                    field: "start_at".into(),
                    pattern: "Start: ([^\\n]+)".into(),
                },
            ],
        }
    }
    #[test]
    fn promotes_replays_and_rejects_ambiguous_receipts() {
        let mut db = Database::open_memory().unwrap();
        let p = package();
        let body = "Title: Paris\nStart: 2026-10-10T10:00:00Z";
        let expected = p.extract(&p.sender, "Confirmed", body).unwrap();
        promote(
            &mut db,
            &p,
            &[Fixture {
                sender: p.sender.clone(),
                subject: "Confirmed".into(),
                body: body.into(),
                expected,
            }],
        )
        .unwrap();
        assert!(known(&db, &p.sender, "Confirmed", body).unwrap().is_some());
        assert!(
            p.extract(&p.sender, "Confirmed", &format!("{body}\n{body}"))
                .is_err()
        );
        assert!(
            known(&db, "attacker@example.test", "Confirmed", body)
                .unwrap()
                .is_none()
        );
        assert!(
            known(
                &db,
                &p.sender,
                "Confirmed",
                "Title: London\nStart: 2027-02-10T10:00:00Z"
            )
            .unwrap()
            .is_some()
        );
        assert!(
            known(&db, &p.sender, "Confirmed", &format!("Cancelled\n{body}"))
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn bad_proposal_does_not_replace_parser() {
        let mut db = Database::open_memory().unwrap();
        let p = package();
        assert!(
            promote(
                &mut db,
                &p,
                &[Fixture {
                    sender: p.sender.clone(),
                    subject: "Confirmed".into(),
                    body: "Title: Paris\nStart: unknown".into(),
                    expected: json!({})
                }]
            )
            .is_err()
        );
        assert_eq!(
            db.conn()
                .query_row("SELECT COUNT(*) FROM learned_parsers", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
