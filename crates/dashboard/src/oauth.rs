//! Direct Gmail OAuth; all application configuration and refresh tokens are local.
use crate::state::AppState;
use anyhow::{Result, ensure};
use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use envelope_email_store::{Database, credential_store, crypto};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
pub struct Config {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub redirect_uri: String,
}
fn config() -> Result<Config> {
    let config: Config = serde_json::from_slice(&std::fs::read(
        envelope_email_store::app_data_dir().join("gmail-oauth.json"),
    )?)?;
    let url = url::Url::parse(&config.redirect_uri)?;
    ensure!(
        url.scheme() == "https"
            || (url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))),
        "OAuth callback must use HTTPS or desktop loopback"
    );
    Ok(config)
}
pub fn initialize(db: &Database) -> Result<()> {
    db.conn().execute_batch("CREATE TABLE IF NOT EXISTS gmail_oauth_pending(state TEXT PRIMARY KEY,verifier TEXT NOT NULL,expires INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS gmail_oauth_accounts(id TEXT PRIMARY KEY,email TEXT NOT NULL,refresh TEXT NOT NULL,page_token TEXT,
      last_sync TEXT,status TEXT NOT NULL DEFAULT 'ready');
      CREATE TABLE IF NOT EXISTS gmail_oauth_seen(account_id TEXT NOT NULL,message_id TEXT NOT NULL,PRIMARY KEY(account_id,message_id));")?;
    Ok(())
}
fn random() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
fn reply(result: Result<Value>) -> Response {
    match result{Ok(v)=>Json(v).into_response(),Err(_)=>(StatusCode::BAD_REQUEST,Json(json!({"error":"gmail_oauth_failed","message":"Check your local OAuth configuration or reconnect Gmail."}))).into_response()}
}
pub async fn start(State(state): State<AppState>) -> Response {
    reply(
        async {
            let c = config()?;
            let nonce = random();
            let verifier = random();
            let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
            let db = state.db.lock().await;
            initialize(&db)?;
            db.conn().execute(
                "INSERT INTO gmail_oauth_pending VALUES(?1,?2,?3)",
                rusqlite::params![
                    format!("{:x}", Sha256::digest(nonce.as_bytes())),
                    verifier,
                    chrono::Utc::now().timestamp() + 600
                ],
            )?;
            let mut url = url::Url::parse("https://accounts.google.com/o/oauth2/v2/auth")?;
            url.query_pairs_mut().extend_pairs([
                ("client_id", c.client_id.as_str()),
                ("redirect_uri", &c.redirect_uri),
                ("response_type", "code"),
                ("scope", "https://www.googleapis.com/auth/gmail.readonly"),
                ("access_type", "offline"),
                ("prompt", "consent"),
                ("state", &nonce),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
            ]);
            Ok(json!({"authorization_url":url.as_str()}))
        }
        .await,
    )
}
#[derive(Deserialize)]
pub struct Callback {
    state: String,
    code: String,
}
pub async fn callback(State(state): State<AppState>, Query(query): Query<Callback>) -> Response {
    reply(async{
        let c=config()?;
        let verifier={
            let mut db=state.db.lock().await;initialize(&db)?;
            let tx=db.conn_mut().transaction()?;
            let hash=format!("{:x}",Sha256::digest(query.state.as_bytes()));
            let verifier:String=tx.query_row("SELECT verifier FROM gmail_oauth_pending WHERE state=?1 AND expires>?2",rusqlite::params![hash,chrono::Utc::now().timestamp()],|r|r.get(0))?;
            tx.execute("DELETE FROM gmail_oauth_pending WHERE state=?1",[hash])?;tx.commit()?;verifier
        };
        let mut form=vec![("client_id",c.client_id.clone()),("redirect_uri",c.redirect_uri),("grant_type","authorization_code".into()),("code",query.code),("code_verifier",verifier)];
        if let Some(secret)=c.client_secret{form.push(("client_secret",secret));}
        let token=exchange(&form).await?;
        let access=token["access_token"].as_str().ok_or_else(||anyhow::anyhow!("No access token"))?;
        let profile=get("https://gmail.googleapis.com/gmail/v1/users/me/profile",access).await?;
        let email=profile["emailAddress"].as_str().ok_or_else(||anyhow::anyhow!("No email"))?;
        let refresh=token["refresh_token"].as_str().ok_or_else(||anyhow::anyhow!("Consent did not supply refresh token"))?;
        let pass=credential_store::get_or_create_passphrase(state.backend)?;
        let encrypted=crypto::encrypt(refresh,&pass)?;
        let db=state.db.lock().await;
        db.conn().execute("INSERT INTO gmail_oauth_accounts(id,email,refresh) VALUES(?1,?1,?2) ON CONFLICT(id) DO UPDATE SET refresh=excluded.refresh,status='ready'",rusqlite::params![email,encrypted])?;
        Ok(json!({"status":"connected","email":email,"next":"/travel"}))
    }.await)
}
async fn read(mut response: reqwest::Response) -> Result<Value> {
    ensure!(response.status().is_success(), "Gmail rejected request");
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= 30 * 1024 * 1024,
            "Gmail response too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()?)
}
async fn exchange(form: &[(&str, String)]) -> Result<Value> {
    read(
        client()?
            .post("https://oauth2.googleapis.com/token")
            .form(form)
            .send()
            .await?,
    )
    .await
}
async fn get(url: &str, token: &str) -> Result<Value> {
    read(client()?.get(url).bearer_auth(token).send().await?).await
}

pub async fn synchronize(state: &AppState) -> Result<()> {
    let accounts = {
        let db = state.db.lock().await;
        initialize(&db)?;
        let mut stmt = db.conn().prepare(
            "SELECT id,refresh,page_token FROM gmail_oauth_accounts WHERE status='ready'",
        )?;
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    };
    if accounts.is_empty() {
        return Ok(());
    }
    let c = config()?;
    let pass = credential_store::get_or_create_passphrase(state.backend)?;
    for (id, encrypted, page) in accounts {
        let mut form = vec![
            ("client_id", c.client_id.clone()),
            ("grant_type", "refresh_token".into()),
            ("refresh_token", crypto::decrypt(&encrypted, &pass)?),
        ];
        if let Some(secret) = &c.client_secret {
            form.push(("client_secret", secret.clone()));
        }
        let value = exchange(&form).await?;
        let access = value["access_token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing access token"))?;
        // Always poll newest messages before advancing historical pagination.
        let mut pages = vec![None];
        if page.is_some() {
            pages.push(page.as_deref());
        }
        for cursor in pages {
            let mut url =
                url::Url::parse("https://gmail.googleapis.com/gmail/v1/users/me/messages")?;
            url.query_pairs_mut()
                .append_pair("maxResults", "50")
                .append_pair("q", "newer_than:1y");
            if let Some(cursor) = cursor {
                url.query_pairs_mut().append_pair("pageToken", cursor);
            }
            let batch = get(url.as_str(), access).await?;
            for message in batch["messages"].as_array().into_iter().flatten() {
                let mid = message["id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Missing message ID"))?;
                ensure!(
                    mid.bytes().all(|b| b.is_ascii_hexdigit()),
                    "Invalid Gmail message ID"
                );
                let seen = {
                    let db = state.db.lock().await;
                    db.conn().query_row("SELECT EXISTS(SELECT 1 FROM gmail_oauth_seen WHERE account_id=?1 AND message_id=?2)",rusqlite::params![id,mid],|r|r.get::<_,bool>(0))?
                };
                if seen {
                    continue;
                }
                let raw = get(
                    &format!(
                        "https://gmail.googleapis.com/gmail/v1/users/me/messages/{mid}?format=raw"
                    ),
                    access,
                )
                .await?;
                let bytes = URL_SAFE_NO_PAD.decode(
                    raw["raw"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("No raw body"))?,
                )?;
                crate::documents::preserve(&bytes, "message/rfc822")?;
                let received = raw["internalDate"]
                    .as_str()
                    .and_then(|v| v.parse::<i64>().ok())
                    .and_then(chrono::DateTime::from_timestamp_millis)
                    .map(|v| v.to_rfc3339());
                crate::handlers::travel::ingest_external(state, &id, mid, &bytes, received).await?;
                let db = state.db.lock().await;
                db.conn().execute(
                    "INSERT OR IGNORE INTO gmail_oauth_seen VALUES(?1,?2)",
                    rusqlite::params![id, mid],
                )?;
            }
            if cursor.is_some() || page.is_none() {
                let db = state.db.lock().await;
                db.conn().execute(
                    "UPDATE gmail_oauth_accounts SET page_token=?2,last_sync=?3 WHERE id=?1",
                    rusqlite::params![
                        id,
                        batch["nextPageToken"].as_str(),
                        chrono::Utc::now().to_rfc3339()
                    ],
                )?;
            }
        }
    }
    Ok(())
}
