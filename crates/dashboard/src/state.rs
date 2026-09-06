// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Shared dashboard state: the SQLite database plus a per-account IMAP
//! connection pool so we don't reconnect on every request.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use envelope_email_store::models::AccountWithCredentials;
use envelope_email_store::{CredentialBackend, Database};
use tokio::sync::{Mutex, OwnedMutexGuard};

use envelope_email_transport::ImapClient;
use envelope_email_transport::imap;

use crate::auth::AuthConfig;
use crate::events::EventBus;

/// Shared application state injected into every handler.
#[derive(Clone)]
pub struct AppState {
    pub browser_handoffs: Arc<Mutex<crate::browser_handoff::Handoffs>>,
    pub db: Arc<Mutex<Database>>,
    pub imap_pool: Arc<Mutex<HashMap<String, Arc<Mutex<ImapClient>>>>>,
    travel_receipt_operations: Arc<Mutex<HashMap<String, Weak<Mutex<()>>>>>,
    pub backend: CredentialBackend,
    /// Gmail onboarding accepts a reusable app password, so it is available
    /// only on a loopback listener (including a loopback HTTPS/Tailscale
    /// reverse proxy). Direct non-loopback HTTP serving disables it.
    pub travel_secret_onboarding_allowed: bool,
    /// Authentication policy for exposure beyond loopback. Defaults to
    /// disabled (open loopback mode); the serve entrypoint injects the resolved
    /// policy via [`AppState::with_auth`].
    pub auth: AuthConfig,
    /// Real-time event bus fanned out to `GET /api/events/stream` subscribers.
    /// Cloneable and cheap; publishing when there are no subscribers is a no-op.
    pub events: EventBus,
}

impl AppState {
    pub fn new(db: Database, backend: CredentialBackend) -> Self {
        Self {
            browser_handoffs: Arc::new(Mutex::new(crate::browser_handoff::Handoffs::default())),
            db: Arc::new(Mutex::new(db)),
            imap_pool: Arc::new(Mutex::new(HashMap::new())),
            travel_receipt_operations: Arc::new(Mutex::new(HashMap::new())),
            backend,
            travel_secret_onboarding_allowed: true,
            auth: AuthConfig::disabled(),
            events: EventBus::new(),
        }
    }

    /// Attach a resolved authentication policy (builder style).
    pub fn with_auth(mut self, auth: AuthConfig) -> Self {
        self.auth = auth;
        self
    }

    /// Constrain credential-bearing onboarding to a safe transport boundary.
    pub fn with_travel_secret_onboarding(mut self, allowed: bool) -> Self {
        self.travel_secret_onboarding_allowed = allowed;
        self
    }

    /// Resolve credentials for an account and return an Arc-Mutex-wrapped IMAP
    /// client, reusing a pooled connection if one exists.
    ///
    /// If the pooled connection fails (e.g., the server dropped it), a fresh
    /// one is created. Callers must hold the returned `Arc<Mutex<ImapClient>>`
    /// only briefly — serializing access per account is acceptable for a
    /// localhost single-user dashboard but will bottleneck under concurrent
    /// requests to the same account.
    pub async fn get_or_create_imap(
        &self,
        account_id: &str,
    ) -> anyhow::Result<(Arc<Mutex<ImapClient>>, AccountWithCredentials)> {
        // Fetch credentials fresh every time (they may have changed).
        let creds = self.resolve_credentials(account_id).await?;

        let mut pool = self.imap_pool.lock().await;
        if let Some(existing) = pool.get(account_id).cloned() {
            return Ok((existing, creds));
        }

        let client = imap::connect(&creds)
            .await
            .map_err(|e| anyhow::anyhow!("IMAP connect failed for {account_id}: {e}"))?;
        let arc = Arc::new(Mutex::new(client));
        pool.insert(account_id.to_string(), arc.clone());
        Ok((arc, creds))
    }

    /// Evict a cached IMAP connection (call when you detect a stale one).
    pub async fn evict_imap(&self, account_id: &str) {
        let mut pool = self.imap_pool.lock().await;
        pool.remove(account_id);
    }

    /// Serialize receipt-derived writes and terminal actions for one durable
    /// receipt. Weak entries keep the keyed lock table self-cleaning after the
    /// final waiter leaves. The guard is intentionally in-memory: after a
    /// process crash a still-pending receipt remains immediately retryable.
    pub async fn lock_travel_receipt_operation(&self, receipt_id: &str) -> OwnedMutexGuard<()> {
        let receipt_lock = {
            let mut locks = self.travel_receipt_operations.lock().await;
            locks.retain(|_, lock| lock.strong_count() > 0);
            if let Some(existing) = locks.get(receipt_id).and_then(Weak::upgrade) {
                existing
            } else {
                let created = Arc::new(Mutex::new(()));
                locks.insert(receipt_id.to_string(), Arc::downgrade(&created));
                created
            }
        };
        receipt_lock.lock_owned().await
    }

    async fn resolve_credentials(
        &self,
        account_id: &str,
    ) -> anyhow::Result<AccountWithCredentials> {
        let passphrase =
            envelope_email_store::credential_store::get_or_create_passphrase(self.backend)
                .map_err(|e| anyhow::anyhow!("credential store error: {e}"))?;

        let db = self.db.lock().await;
        // Try ID, then email lookup
        if let Some(acct) = db
            .get_account(account_id)
            .map_err(|e| anyhow::anyhow!("db error: {e}"))?
        {
            return db
                .get_account_with_credentials(&acct.id, &passphrase)
                .map_err(|e| anyhow::anyhow!("decrypt credentials for {account_id}: {e}"));
        }
        if let Some(acct) = db
            .find_account_by_email(account_id)
            .map_err(|e| anyhow::anyhow!("db error: {e}"))?
        {
            return db
                .get_account_with_credentials(&acct.id, &passphrase)
                .map_err(|e| anyhow::anyhow!("decrypt credentials for {account_id}: {e}"));
        }
        anyhow::bail!("account not found: {account_id}")
    }
}
