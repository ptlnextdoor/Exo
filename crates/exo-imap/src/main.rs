//! `exo-imap`: serves an IMAP mailbox as Gmail on loopback until stopped.
//!
//! The password comes from `EXO_IMAP_PASSWORD` (never an argument, which `ps` shows).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use exo_imap::Facade;
use tokio::net::TcpListener;
use tokio::sync::Notify;

/// Longest the facade goes without asking the server what changed.
const FALLBACK: Duration = Duration::from_secs(300);

/// Wakes `wake` when the server reports a change to the inbox. Reconnects after a failure.
async fn watch((host, port, user, password): (String, u16, String, String), wake: Arc<Notify>) {
    loop {
        let wait = async {
            exo_imap::imap::connect(&host, port, &user, &password)
                .await?
                .idle(Duration::from_secs(29 * 60))
                .await
        };
        match wait.await {
            Ok(()) => wake.notify_one(),
            Err(error) => {
                log::warn!("waiting for new mail failed: {error:#}");
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
    }
}

#[derive(Parser)]
struct Args {
    /// IMAP server, reached over TLS.
    #[arg(long, default_value = "imap.purelymail.com")]
    host: String,
    #[arg(long, default_value_t = 993)]
    port: u16,
    /// Outgoing mail server, reached over TLS on port 465.
    #[arg(long, default_value = "smtp.purelymail.com")]
    smtp_host: String,
    /// The login, which is also the account's address.
    #[arg(long)]
    user: String,
    /// The account's display name.
    #[arg(long, default_value = "")]
    name: String,
    /// Where to keep the index, so IDs survive a restart.
    #[arg(long)]
    index: PathBuf,
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: String,
    /// Print what the server offers (capabilities, folders, counts, no mail) and stop.
    #[arg(long)]
    probe: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // `async_imap` logs every command it sends, `LOGIN` with the password included.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .filter_module("async_imap", log::LevelFilter::Off)
        .init();
    let args = Args::parse();
    let password = std::env::var("EXO_IMAP_PASSWORD").context("EXO_IMAP_PASSWORD is not set")?;

    let mut store = exo_imap::imap::connect(&args.host, args.port, &args.user, &password).await?;
    if args.probe {
        print!("{}", store.probe().await?);
        return Ok(());
    }
    let store = store.with_smtp(exo_imap::imap::smtp(
        &args.smtp_host,
        &args.user,
        &password,
    )?);
    let facade = Arc::new(Facade::open(
        store,
        Some(&args.index),
        &args.user,
        &args.name,
    )?);
    log::info!("first sync: {:?}", facade.sync().await?);

    // The server says when mail arrives; Exo's own polling is the fallback.
    let wake = Arc::new(Notify::new());
    tokio::spawn(watch(
        (args.host.clone(), args.port, args.user.clone(), password),
        wake.clone(),
    ));
    tokio::spawn({
        let facade = facade.clone();
        async move {
            loop {
                tokio::select! {
                    () = wake.notified() => {}
                    () = tokio::time::sleep(FALLBACK) => {}
                }
                match facade.sync_if_stale().await {
                    Ok(report) if report.changed() => log::info!("synced: {report:?}"),
                    Ok(_) => {}
                    Err(error) => log::warn!("sync failed: {error:#}"),
                }
            }
        }
    });

    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).map_err(|error| anyhow::anyhow!("no randomness: {error}"))?;
    let token: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
    let listener = TcpListener::bind(&args.listen).await?;
    // The one place the token is shown: whoever starts this passes it to the client.
    println!("http://{} {token}", listener.local_addr()?);
    axum::serve(listener, exo_imap::router(facade, &token)).await?;
    Ok(())
}
