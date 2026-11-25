//! `exo-imap`: serves an IMAP mailbox as Gmail on loopback until stopped.
//!
//! The password comes from `EXO_IMAP_PASSWORD` (never an argument, which `ps` shows).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use exo_imap::Facade;
use tokio::net::TcpListener;

#[derive(Parser)]
struct Args {
    /// IMAP server, reached over TLS.
    #[arg(long, default_value = "imap.purelymail.com")]
    host: String,
    #[arg(long, default_value_t = 993)]
    port: u16,
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
}

#[tokio::main]
async fn main() -> Result<()> {
    // `async_imap` logs every command it sends, `LOGIN` with the password included.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .filter_module("async_imap", log::LevelFilter::Off)
        .init();
    let args = Args::parse();
    let password = std::env::var("EXO_IMAP_PASSWORD").context("EXO_IMAP_PASSWORD is not set")?;

    let store = exo_imap::imap::connect(&args.host, args.port, &args.user, &password).await?;
    let facade = Arc::new(Facade::open(
        store,
        Some(&args.index),
        &args.user,
        &args.name,
    )?);
    log::info!("first sync: {:?}", facade.sync().await?);

    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).map_err(|error| anyhow::anyhow!("no randomness: {error}"))?;
    let token: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
    let listener = TcpListener::bind(&args.listen).await?;
    // The one place the token is shown: whoever starts this passes it to the client.
    println!("http://{} {token}", listener.local_addr()?);
    axum::serve(listener, exo_imap::router(facade, &token)).await?;
    Ok(())
}
