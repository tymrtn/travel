//! Portable, credential-free directory snapshots. Restore never overwrites data.
use anyhow::{Result, ensure};
use envelope_email_store::Database;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::Path,
};

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    files: BTreeMap<String, String>,
    credentials: String,
}
fn digest(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn allowed(name: &str) -> bool {
    matches!(name, "travel.db" | "intelligence.json")
        || name
            .strip_prefix("documents/")
            .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
}
fn copy_new(source: &Path, destination: &Path) -> Result<()> {
    ensure!(
        std::fs::symlink_metadata(source)?.file_type().is_file(),
        "Snapshot files must be regular files"
    );
    let mut input = std::fs::File::open(source)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(destination)?;
    std::io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    Ok(())
}
fn private_directory(path: &Path) -> Result<()> {
    std::fs::create_dir(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn export(source_root: &Path, output: &Path) -> Result<usize> {
    ensure!(!output.exists(), "Choose a new backup directory");
    let source = Connection::open_with_flags(
        source_root.join("travel.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    private_directory(output)?;
    let snapshot = output.join("travel.db");
    source.execute("VACUUM INTO ?1", [snapshot.to_string_lossy().as_ref()])?;
    let sanitized = Connection::open(&snapshot)?;
    sanitized.execute_batch("PRAGMA secure_delete=ON; UPDATE accounts SET encrypted_password='',encrypted_smtp_password=NULL,encrypted_imap_password=NULL; DELETE FROM travel_share_links;")?;
    // Sessions, browser device endpoints, OAuth grants, and VAPID secrets are
    // capabilities, not itinerary data. Re-enroll them on the restored origin.
    for table in [
        "household_sessions",
        "household_members",
        "gmail_oauth_pending",
        "gmail_oauth_accounts",
        "push_configuration",
        "push_devices",
        "push_outbox",
    ] {
        let exists = sanitized.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |r| r.get::<_, bool>(0),
        )?;
        if exists {
            sanitized.execute(&format!("DELETE FROM {table}"), [])?;
        }
    }
    sanitized.execute_batch("VACUUM;")?;
    drop(sanitized);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(0o600))?;
    }
    let mut files = BTreeMap::new();
    files.insert("travel.db".into(), digest(&snapshot)?);
    let config = source_root.join("intelligence.json");
    if config.exists() {
        copy_new(&config, &output.join("intelligence.json"))?;
        files.insert("intelligence.json".into(), digest(&config)?);
    }
    if source_root.join("documents").exists() {
        ensure!(
            std::fs::symlink_metadata(source_root.join("documents"))?
                .file_type()
                .is_dir(),
            "Document directory cannot be a symlink"
        );
        private_directory(&output.join("documents"))?;
        for entry in std::fs::read_dir(source_root.join("documents"))? {
            let entry = entry?;
            let name = format!("documents/{}", entry.file_name().to_string_lossy());
            ensure!(allowed(&name), "Unexpected file in document store");
            let hash = digest(&entry.path())?;
            ensure!(
                name == format!("documents/{hash}"),
                "Document hash mismatch"
            );
            copy_new(&entry.path(), &output.join(&name))?;
            files.insert(name, hash);
        }
    }
    let count = files.len();
    let manifest = Manifest {
        version: 1,
        files,
        credentials: "excluded; reconnect integrations and reissue sharing and enrollment links"
            .into(),
    };
    // Manifest is the completion marker. Interrupted exports cannot restore.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output.join("manifest.json"))?;
    file.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    file.sync_all()?;
    Ok(count)
}

pub fn restore(source: &Path, destination: &Path) -> Result<usize> {
    ensure!(
        !destination.exists(),
        "Restore requires a new, nonexistent Travel data directory"
    );
    let encoded = std::fs::read(source.join("manifest.json"))?;
    ensure!(
        encoded.len() <= 16 * 1024 * 1024,
        "Snapshot manifest too large"
    );
    let manifest: Manifest = serde_json::from_slice(&encoded)?;
    ensure!(
        manifest.version == 1 && manifest.files.contains_key("travel.db"),
        "Unsupported or incomplete snapshot"
    );
    for (name, hash) in &manifest.files {
        ensure!(allowed(name), "Invalid snapshot path");
        if name.starts_with("documents/") {
            ensure!(
                std::fs::symlink_metadata(source.join("documents"))?
                    .file_type()
                    .is_dir(),
                "Snapshot document directory cannot be a symlink"
            );
        }
        ensure!(
            std::fs::symlink_metadata(source.join(name))?
                .file_type()
                .is_file(),
            "Snapshot symlink not allowed"
        );
        ensure!(
            digest(&source.join(name))? == *hash,
            "Snapshot integrity mismatch"
        );
    }
    // Run schema validation on an isolated copy, never against the source.
    let staging = tempfile::tempdir()?;
    copy_new(&source.join("travel.db"), &staging.path().join("travel.db"))?;
    let db = Database::open(&staging.path().join("travel.db"))?;
    let integrity: String = db
        .conn()
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    ensure!(integrity == "ok", "Database integrity check failed");
    drop(db);
    private_directory(destination)?;
    private_directory(&destination.join("documents"))?;
    // The database is copied last so an interrupted restore never appears ready.
    for name in manifest
        .files
        .keys()
        .filter(|name| name.as_str() != "travel.db")
    {
        copy_new(&source.join(name), &destination.join(name))?;
    }
    copy_new(
        &staging.path().join("travel.db"),
        &destination.join("travel.db"),
    )?;
    Ok(manifest.files.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backup_roundtrip_preserves_identity_and_rejects_tampering() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        std::fs::create_dir(&original).unwrap();
        let db = Database::open(&original.join("travel.db")).unwrap();
        db.create_trip(&envelope_email_store::NewTrip {
            id: "stable-id".into(),
            title: "Test itinerary".into(),
            destination: None,
            starts_at: None,
            ends_at: None,
            timezone: "UTC".into(),
            status: "planning".into(),
            notes: None,
            now: "2026-09-01T00:00:00Z".into(),
        })
        .unwrap();
        crate::household::initialize(&db).unwrap();
        db.conn().execute("INSERT INTO household_members VALUES('m','private','viewer','[]','secret-token-hash',0)",[]).unwrap();
        let backup = temp.path().join("backup");
        export(&original, &backup).unwrap();
        let restored = temp.path().join("restored");
        restore(&backup, &restored).unwrap();
        let restored_db = Database::open(&restored.join("travel.db")).unwrap();
        assert_eq!(restored_db.list_trips().unwrap()[0].id, "stable-id");
        assert_eq!(
            restored_db
                .conn()
                .query_row("SELECT COUNT(*) FROM household_members", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(restore(&backup, &restored).is_err());
        std::fs::OpenOptions::new()
            .append(true)
            .open(backup.join("travel.db"))
            .unwrap()
            .write_all(b"tamper")
            .unwrap();
        assert!(restore(&backup, &temp.path().join("tampered")).is_err());
        assert!(!temp.path().join("tampered").exists());
    }
}
