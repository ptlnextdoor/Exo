//! [`Mailstore`] over `async-imap`.

use std::fmt::Debug;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_imap::types::{Capability, Fetch, Flag, NameAttribute};
use async_imap::{Client, Session};
use futures::TryStreamExt;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, crypto::ring};

use crate::mailstore::{FolderInfo, FolderState, Header, Mailstore};

/// UIDs per `FETCH`, so no command line grows past what servers accept.
const FETCH_BATCH: usize = 250;

pub struct ImapStore<T: AsyncRead + AsyncWrite + Unpin + Send + Debug> {
    session: Session<T>,
    condstore: bool,
}

/// Connects to `host:port` over implicit TLS (never plaintext) and logs in.
pub async fn connect(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
) -> Result<ImapStore<TlsStream<TcpStream>>> {
    let config = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()?;
    let config = rustls_platform_verifier::BuilderVerifierExt::with_platform_verifier(config)?
        .with_no_client_auth();
    let name = ServerName::try_from(host.to_owned())?;
    let tcp = TcpStream::connect((host, port))
        .await
        .with_context(|| format!("connecting to {host}:{port}"))?;
    let tls = TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await?;
    login(tls, user, password).await
}

/// Logs in over an established stream. [`connect`] is the only way in outside tests, because a
/// caller of this brings its own transport.
pub async fn login<T: AsyncRead + AsyncWrite + Unpin + Send + Debug>(
    stream: T,
    user: &str,
    password: &str,
) -> Result<ImapStore<T>> {
    let mut client = Client::new(stream);
    client
        .read_response()
        .await?
        .ok_or_else(|| anyhow!("the server closed the connection before it greeted"))?;
    // Never `?` the pair itself: the error only carries the client, not the password.
    let mut session = client
        .login(user, password)
        .await
        .map_err(|(error, _)| anyhow!("IMAP login failed: {error}"))?;
    let condstore = session
        .capabilities()
        .await?
        .has(&Capability::Atom("CONDSTORE".into()));
    Ok(ImapStore { session, condstore })
}

fn flag_name(flag: Flag<'_>) -> Option<String> {
    Some(match flag {
        Flag::Seen => "\\Seen".into(),
        Flag::Answered => "\\Answered".into(),
        Flag::Flagged => "\\Flagged".into(),
        Flag::Deleted => "\\Deleted".into(),
        Flag::Draft => "\\Draft".into(),
        Flag::Custom(name) => name.into_owned(),
        Flag::Recent | Flag::MayCreate => return None,
    })
}

fn flags_of(fetch: &Fetch) -> Vec<String> {
    fetch.flags().filter_map(flag_name).collect()
}

fn uid_set(uids: &[u32]) -> String {
    uids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + Debug> Mailstore for ImapStore<T> {
    async fn folders(&mut self) -> Result<Vec<FolderInfo>> {
        let names: Vec<_> = self
            .session
            .list(Some(""), Some("*"))
            .await?
            .try_collect()
            .await?;
        Ok(names
            .iter()
            .map(|name| FolderInfo {
                name: name.name().to_owned(),
                delimiter: name.delimiter().map(str::to_owned),
                attributes: name
                    .attributes()
                    .iter()
                    .map(|attribute| match attribute {
                        NameAttribute::NoInferiors => "\\Noinferiors".to_owned(),
                        NameAttribute::NoSelect => "\\Noselect".to_owned(),
                        NameAttribute::Marked => "\\Marked".to_owned(),
                        NameAttribute::Unmarked => "\\Unmarked".to_owned(),
                        NameAttribute::Extension(name) => name.to_string(),
                        _ => String::new(),
                    })
                    .collect(),
            })
            .collect())
    }

    async fn select(&mut self, folder: &str) -> Result<FolderState> {
        let mailbox = if self.condstore {
            self.session.select_condstore(folder).await
        } else {
            self.session.select(folder).await
        }
        .with_context(|| format!("selecting {folder}"))?;
        Ok(FolderState {
            uid_validity: mailbox
                .uid_validity
                .context("the server sent no UIDVALIDITY")?,
            uid_next: mailbox.uid_next.context("the server sent no UIDNEXT")?,
            highest_modseq: mailbox.highest_modseq,
        })
    }

    async fn uids(&mut self) -> Result<Vec<u32>> {
        let mut uids: Vec<u32> = self.session.uid_search("ALL").await?.into_iter().collect();
        uids.sort_unstable();
        Ok(uids)
    }

    async fn headers(&mut self, uids: &[u32]) -> Result<Vec<Header>> {
        let mut headers = Vec::with_capacity(uids.len());
        for batch in uids.chunks(FETCH_BATCH) {
            let fetches: Vec<Fetch> = self
                .session
                .uid_fetch(
                    uid_set(batch),
                    "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER])",
                )
                .await?
                .try_collect()
                .await?;
            for fetch in &fetches {
                // A message that was expunged meanwhile just isn't answered.
                let (Some(uid), Some(header)) = (fetch.uid, fetch.header()) else {
                    continue;
                };
                headers.push(Header {
                    uid,
                    flags: flags_of(fetch),
                    internal_date: fetch.internal_date().map_or(0, |d| d.timestamp_millis()),
                    size: fetch.size.unwrap_or_default(),
                    header: header.to_vec(),
                });
            }
        }
        Ok(headers)
    }

    async fn flags(&mut self, changed_since: Option<u64>) -> Result<Vec<(u32, Vec<String>)>> {
        let query = match changed_since {
            Some(modseq) => format!("(FLAGS) (CHANGEDSINCE {modseq})"),
            None => "(FLAGS)".to_owned(),
        };
        let fetches: Vec<Fetch> = self
            .session
            .uid_fetch("1:*", query)
            .await?
            .try_collect()
            .await?;
        Ok(fetches
            .iter()
            .filter_map(|fetch| Some((fetch.uid?, flags_of(fetch))))
            .collect())
    }

    async fn body(&mut self, uid: u32) -> Result<Vec<u8>> {
        let fetches: Vec<Fetch> = self
            .session
            .uid_fetch(uid.to_string(), "BODY.PEEK[]")
            .await?
            .try_collect()
            .await?;
        fetches
            .iter()
            .find_map(|fetch| fetch.body().map(<[u8]>::to_vec))
            .with_context(|| format!("message {uid} is gone"))
    }
}
