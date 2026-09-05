//! Explicit read-only import of legacy Travel records. Never opens the source
//! through the application's migration runner.
use anyhow::{Result, ensure};
use envelope_email_store::Database;
use rusqlite::{Connection, OpenFlags, params_from_iter};
use serde_json::{Value, json};
use std::path::Path;
const TABLES: &[&str] = &[
    "travel_trips",
    "travel_receipts",
    "travel_bookings",
    "travel_segments",
    "travel_tasks",
    "travel_alerts",
];

pub fn import_snapshot(source: &Path, destination: &mut Database, apply: bool) -> Result<Value> {
    let source = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut counts = serde_json::Map::new();
    for table in TABLES {
        let mut columns=destination.conn().prepare(&format!("PRAGMA table_info({table})"))?;
        let names=columns.query_map([],|r|r.get::<_,String>(1))?.collect::<std::result::Result<Vec<_>,_>>()?;
        let mut source_columns=source.prepare(&format!("PRAGMA table_info({table})"))?;
        let source_names=source_columns.query_map([],|r|r.get::<_,String>(1))?.collect::<std::result::Result<std::collections::HashSet<_>,_>>()?;
        ensure!(names.iter().all(|name|source_names.contains(name)),"Incompatible snapshot columns in {table}");
        let list=names.iter().map(|name|format!("\"{name}\"")).collect::<Vec<_>>().join(",");
        // Compatibility is checked during preview too, before any destination writes.
        source.prepare(&format!("SELECT {list} FROM {table} LIMIT 0"))?;
        let count: i64 =
            source.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
        counts.insert((*table).into(), json!(count));
    }
    for query in [
        "SELECT COUNT(*) FROM travel_bookings b LEFT JOIN travel_trips t ON t.id=b.trip_id WHERE t.id IS NULL",
        "SELECT COUNT(*) FROM travel_segments s LEFT JOIN travel_trips t ON t.id=s.trip_id WHERE t.id IS NULL",
        "SELECT COUNT(*) FROM travel_segments s LEFT JOIN travel_bookings b ON b.id=s.booking_id WHERE s.booking_id IS NOT NULL AND (b.id IS NULL OR b.trip_id<>s.trip_id)",
        "SELECT COUNT(*) FROM travel_tasks s LEFT JOIN travel_trips t ON t.id=s.trip_id WHERE s.trip_id IS NOT NULL AND t.id IS NULL",
    ]{ensure!(source.query_row(query,[],|r|r.get::<_,i64>(0))?==0,"Snapshot contains orphaned or cross-trip records");}
    if !apply {
        return Ok(
            json!({"counts":counts,"applied":false,"credentials":"reconnect","sharing":"reissue","originals":"Legacy receipt originals may not have been retained"}),
        );
    }
    for table in TABLES {
        ensure!(
            destination
                .conn()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))?
                == 0,
            "Destination must have no Travel records"
        );
    }
    let tx = destination.conn_mut().transaction()?;
    tx.execute_batch("PRAGMA defer_foreign_keys=ON;")?;
    for table in TABLES {
        let mut columns = tx.prepare(&format!("PRAGMA table_info({table})"))?;
        let names = columns
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        // Explicit columns deliberately reject incompatible legacy schemas.
        let list = names
            .iter()
            .map(|name| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(",");
        let mut query = source.prepare(&format!("SELECT {list} FROM {table}"))?;
        let mut rows = query.query([])?;
        let placeholders = (0..names.len()).map(|_| "?").collect::<Vec<_>>().join(",");
        while let Some(row) = rows.next()? {
            let values = (0..names.len())
                .map(|i| row.get::<_, rusqlite::types::Value>(i))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            tx.execute(
                &format!("INSERT INTO {table} ({list}) VALUES ({placeholders})"),
                params_from_iter(values),
            )?;
        }
    }
    tx.commit()?;
    Ok(json!({"counts":counts,"applied":true,"credentials":"reconnect","sharing":"reissue"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_does_not_mutate_and_import_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.db");
        let db = Database::open(&source).unwrap();
        db.create_trip(&envelope_email_store::NewTrip {
            id: "trip".into(),
            title: "Test".into(),
            destination: None,
            starts_at: None,
            ends_at: None,
            timezone: "UTC".into(),
            status: "upcoming".into(),
            notes: None,
            now: "2026-09-01T00:00:00Z".into(),
        })
        .unwrap();
        drop(db);
        let mut target = Database::open_memory().unwrap();
        import_snapshot(&source, &mut target, false).unwrap();
        assert!(target.list_trips().unwrap().is_empty());
        import_snapshot(&source, &mut target, true).unwrap();
        assert_eq!(target.list_trips().unwrap().len(), 1);
        assert!(import_snapshot(&source, &mut target, true).is_err());
    }
}
