//! Immutable originals and a common file/EML ingestion boundary.
use crate::{
    handlers::travel::{self, ManualImportRequest},
    state::AppState,
};
use anyhow::{Result, ensure};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Serialize, Deserialize)]
pub struct Document {
    pub version: u32,
    pub hash: String,
    pub bytes: usize,
    pub media_type: String,
}

pub fn preserve(bytes: &[u8], media_type: &str) -> Result<Document> {
    ensure!(bytes.len() <= 20 * 1024 * 1024, "Document too large");
    let hash = format!("{:x}", Sha256::digest(bytes));
    let root = envelope_email_store::app_data_dir().join("documents");
    std::fs::create_dir_all(&root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    }
    let destination = root.join(&hash);
    let mut file = tempfile::NamedTempFile::new_in(&root)?;
    use std::io::Write;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(&destination) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                std::fs::read(destination)? == bytes,
                "Stored document hash mismatch"
            );
        }
        Err(error) => return Err(error.error.into()),
    }
    Ok(Document {
        version: 1,
        hash,
        bytes: bytes.len(),
        media_type: media_type.into(),
    })
}

pub async fn import(
    State(state): State<AppState>,
    Path(filename): Path<String>,
    bytes: Bytes,
) -> Response {
    let media = if filename.ends_with(".eml") {
        "message/rfc822"
    } else if filename.ends_with(".pdf") {
        "application/pdf"
    } else {
        "text/plain"
    };
    let document = match preserve(&bytes, media) {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error":"document_storage_failed"})),
            )
                .into_response();
        }
    };
    let request = if media == "message/rfc822" {
        match crate::travel_parser::parse_raw_travel_email(&bytes) {
            Some(email) => ManualImportRequest {
                account_id: None,
                from_addr: email.from_addr,
                subject: email.subject,
                received_at: email.received_at,
                body_text: email.text_body,
                html_body: email.html_body,
            },
            None => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(serde_json::json!({"error":"unreadable_email","document":document})),
                )
                    .into_response();
            }
        }
    } else if media == "application/pdf" {
        let source = envelope_email_store::app_data_dir()
            .join("documents")
            .join(&document.hash);
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            extract_pdf_text(&source),
        )
        .await;
        match output{
            Ok(Ok(output))=>ManualImportRequest{account_id:None,from_addr:"file@local.invalid".into(),subject:filename,received_at:None,body_text:output,html_body:None},
            _=>return (StatusCode::UNPROCESSABLE_ENTITY,Json(serde_json::json!({"error":"pdf_text_unavailable","document":document,"message":"Original preserved. Install pdftotext or import extracted text."}))).into_response()
        }
    } else {
        match std::str::from_utf8(&bytes) {
            Ok(body) => ManualImportRequest {
                account_id: None,
                from_addr: "file@local.invalid".into(),
                subject: filename,
                received_at: None,
                body_text: body.into(),
                html_body: None,
            },
            Err(_) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(serde_json::json!({"error":"unsupported_document","document":document})),
                )
                    .into_response();
            }
        }
    };
    let mut response = travel::import_receipt(State(state), Json(request)).await;
    response
        .headers_mut()
        .insert("x-travel-document", document.hash.parse().unwrap());
    response
}

async fn extract_pdf_text(source: &std::path::Path) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let mut child = tokio::process::Command::new("pdftotext")
        .arg("-layout")
        .arg(source)
        .arg("-")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(bytes.len() <= 1024 * 1024, "PDF text exceeded limit");
    ensure!(child.wait().await?.success(), "PDF extraction failed");
    let text = String::from_utf8(bytes)?;
    ensure!(!text.trim().is_empty(), "PDF needs OCR or review");
    Ok(text)
}

pub async fn download(Path(hash): Path<String>) -> Response {
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::fs::read(
        envelope_email_store::app_data_dir()
            .join("documents")
            .join(hash),
    )
    .await
    {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream"),
                (header::CONTENT_DISPOSITION, "attachment"),
                (header::CACHE_CONTROL, "private, no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
