use clap::{Parser, Subcommand};
use envelope_email_dashboard::{auth::AuthConfig, state::AppState};
use envelope_email_store::{CredentialBackend, Database};
use std::net::IpAddr;

#[derive(Parser)]
#[command(version, about = "Independent, sovereign travel organizer")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Serve {
        #[arg(long, default_value_t = 3150)]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        bind: IpAddr,
        #[arg(long)]
        no_background_sync: bool,
    },
    Doctor,
    Backup {
        #[arg(long)]
        output: std::path::PathBuf,
    },
    Restore {
        #[arg(long)]
        snapshot: std::path::PathBuf,
    },
    ImportEnvelope {
        #[arg(long)]
        snapshot: std::path::PathBuf,
        #[arg(long)]
        apply: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("warn").init();
    match Cli::parse().command {
        Command::Backup { output } => {
            let files = envelope_email_dashboard::backup::export(
                &envelope_email_store::app_data_dir(),
                &output,
            )?;
            println!(
                "Backed up {files} files. Credentials and access links excluded; reconnect after restore."
            );
        }
        Command::Restore { snapshot } => {
            let files = envelope_email_dashboard::backup::restore(
                &snapshot,
                &envelope_email_store::app_data_dir(),
            )?;
            println!(
                "Restored {files} files. Reconnect integrations and reissue family links before use."
            );
        }
        Command::ImportEnvelope { snapshot, apply } => {
            let mut db = if apply {
                Database::open_default()?
            } else {
                Database::open_memory()?
            };
            println!(
                "{}",
                envelope_email_dashboard::migration::import_snapshot(&snapshot, &mut db, apply)?
            );
        }
        Command::Doctor => {
            println!(
                "{}",
                serde_json::json!({"product":"Travel", "database": envelope_email_store::database_path(), "version":env!("CARGO_PKG_VERSION")})
            );
        }
        Command::Serve {
            port,
            bind,
            no_background_sync,
        } => {
            let auth =
                AuthConfig::from_parts(std::env::var("TRAVEL_TOKEN").ok(), Vec::<String>::new());
            anyhow::ensure!(
                bind.is_loopback() || auth.is_enforced(),
                "Set TRAVEL_TOKEN before exposing Travel beyond loopback."
            );
            let listener = tokio::net::TcpListener::bind((bind,port)).await
                .map_err(|e| anyhow::anyhow!("Cannot listen on {bind}:{port}: {e}. Choose --port or stop the process occupying this port."))?;
            let backend = if cfg!(target_os = "macos") {
                CredentialBackend::Keychain
            } else {
                CredentialBackend::File
            };
            let state = AppState::new(Database::open_default()?, backend)
                .with_auth(auth)
                .with_travel_secret_onboarding(bind.is_loopback());
            if !no_background_sync {
                let worker = state.clone();
                tokio::spawn(async move {
                    let mut timer = tokio::time::interval(std::time::Duration::from_secs(60));
                    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    loop {
                        timer.tick().await;
                        if envelope_email_dashboard::push::deliver(&worker)
                            .await
                            .is_err()
                        {
                            tracing::warn!(
                                "Notification delivery will retry; inspect device settings"
                            );
                        }
                        if envelope_email_dashboard::oauth::synchronize(&worker)
                            .await
                            .is_err()
                        {
                            tracing::warn!(
                                "Gmail OAuth sync failed; reconnect or retry from source settings"
                            );
                        }
                        if envelope_email_dashboard::handlers::travel::background_sync(&worker)
                            .await
                            .is_err()
                        {
                            tracing::warn!(
                                "Travel sync failed; inspect source health and reconnect if necessary"
                            );
                        }
                    }
                });
            }
            println!("Travel running at http://{}/travel", listener.local_addr()?);
            axum_serve(listener, state).await?;
        }
    }
    Ok(())
}

async fn axum_serve(listener: tokio::net::TcpListener, state: AppState) -> anyhow::Result<()> {
    envelope_email_dashboard::run_listener(listener, state).await
}
