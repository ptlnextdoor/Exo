//! [`Mailstore`] over `async-imap`.

use std::fmt::Debug;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_imap::types::{Capability, Fetch, Flag, NameAttribute};
use async_imap::{Client, Session};
use futures::TryStreamExt;
use lettre::address::Envelope;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Address, AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, crypto::ring};

use crate::mailstore::{FolderInfo, FolderState, Header, Mailstore};

/// UIDs per `FETCH`, so no command line grows past what servers accept.
const FETCH_BATCH: usize = 250;

/// The outgoing mail server.
pub type Smtp = AsyncSmtpTransport<Tokio1Executor>;

/// `smtp_host` over implicit TLS (port 465, never plaintext), logging in as `user`.
pub fn smtp(host: &str, user: &str, password: &str) -> Result<Smtp> {
    Ok(Smtp::relay(host)?
        .credentials(Credentials::new(user.to_owned(), password.to_owned()))
        .build())
}

pub struct ImapStore<T: AsyncRead + AsyncWrite + Unpin + Send + Debug> {
    session: Session<T>,
    condstore: bool,
    /// `UID MOVE`, which a server without it makes `COPY`, `STORE` and `EXPUNGE`.
    can_move: bool,
    /// `UID EXPUNGE`, which a server without it makes plain `EXPUNGE`.
    uid_plus: bool,
    selected: Option<String>,
    smtp: Option<Smtp>,
}

/// Connects to `host:port` over implicit TLS (never plaintext) and logs in.
pub async fn connect(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
) -> Result<ImapStore<TlsStream<TcpStream>>> {
    let tls = tls(host, port).await?;
    login(tls, user, password).await
}

async fn tls(host: &str, port: u16) -> Result<TlsStream<TcpStream>> {
    let config = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()?;
    let config = rustls_platform_verifier::BuilderVerifierExt::with_platform_verifier(config)?
        .with_no_client_auth();
    let name = ServerName::try_from(host.to_owned())?;
    let tcp = TcpStream::connect((host, port))
        .await
        .with_context(|| format!("connecting to {host}:{port}"))?;
    Ok(TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await?)
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
    let capabilities = session.capabilities().await?;
    let has = |name: &str| capabilities.has(&Capability::Atom(name.into()));
    Ok(ImapStore {
        condstore: has("CONDSTORE"),
        can_move: has("MOVE"),
        uid_plus: has("UIDPLUS"),
        session,
        selected: None,
        smtp: None,
    })
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + Debug> ImapStore<T> {
    /// Sends mail through `smtp`.
    pub fn with_smtp(mut self, smtp: Smtp) -> Self {
        self.smtp = Some(smtp);
        self
    }

    /// Moves by `COPY`, `STORE` and `EXPUNGE` as for a server without `MOVE`.
    pub fn without_move(mut self) -> Self {
        self.can_move = false;
        self
    }

    /// What the server offers and holds, for a first look at a mailbox: its capabilities, and
    /// for each folder its flags and how many messages it has. It names no message.
    pub async fn probe(&mut self) -> Result<String> {
        use std::fmt::Write;
        let mut out = String::new();
        let capabilities = self.session.capabilities().await?;
        let names: Vec<String> = capabilities.iter().map(|c| format!("{c:?}")).collect();
        writeln!(out, "capabilities: {}", names.join(" "))?;
        for folder in self.folders().await? {
            let mailbox = self.session.select(&folder.name).await?;
            let permanent: Vec<String> = mailbox
                .permanent_flags
                .iter()
                .map(|flag| format!("{flag:?}"))
                .collect();
            writeln!(
                out,
                "{:?} {:?} messages={} uidvalidity={:?} modseq={:?} permanent={}",
                folder.name,
                folder.attributes,
                mailbox.exists,
                mailbox.uid_validity,
                mailbox.highest_modseq,
                permanent.join(" ")
            )?;
        }
        Ok(out)
    }

    /// Waits on `INBOX` until the server says something changed, or `patience` passes. A
    /// connection of its own does this, since IDLE ties up the one it runs on.
    pub async fn idle(mut self, patience: std::time::Duration) -> Result<()> {
        self.session.select("INBOX").await?;
        let mut handle = self.session.idle();
        handle.init().await?;
        let (wait, _stop) = handle.wait_with_timeout(patience);
        wait.await?;
        Ok(())
    }
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
        self.selected = Some(folder.to_owned());
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

    async fn search(&mut self, query: &str) -> Result<Vec<u32>> {
        let mut uids: Vec<u32> = self.session.uid_search(query).await?.into_iter().collect();
        uids.sort_unstable();
        Ok(uids)
    }

    async fn create(&mut self, folder: &str) -> Result<()> {
        Ok(self.session.create(folder).await?)
    }

    async fn store_flags(&mut self, uids: &[u32], add: &[String], remove: &[String]) -> Result<()> {
        for (sign, flags) in [('+', add), ('-', remove)] {
            if flags.is_empty() {
                continue;
            }
            let query = format!("{sign}FLAGS.SILENT ({})", flags.join(" "));
            for batch in uids.chunks(FETCH_BATCH) {
                let _: Vec<Fetch> = self
                    .session
                    .uid_store(uid_set(batch), &query)
                    .await?
                    .try_collect()
                    .await?;
            }
        }
        Ok(())
    }

    async fn move_to(&mut self, uids: &[u32], to: &str) -> Result<Vec<Header>> {
        let from = self.selected.clone().context("nothing is selected")?;
        // Anything at or after this UID in `to` arrived by the move, or by delivery meanwhile.
        let next = self.select(to).await?.uid_next;
        self.select(&from).await?;
        for batch in uids.chunks(FETCH_BATCH) {
            let set = uid_set(batch);
            if self.can_move {
                self.session.uid_mv(&set, to).await?;
            } else {
                self.session.uid_copy(&set, to).await?;
                self.expunge(batch).await?;
            }
        }
        self.select(to).await?;
        let arrived: Vec<u32> = self
            .uids()
            .await?
            .into_iter()
            .filter(|uid| *uid >= next)
            .collect();
        self.headers(&arrived).await
    }

    async fn expunge(&mut self, uids: &[u32]) -> Result<()> {
        for batch in uids.chunks(FETCH_BATCH) {
            self.store_flags(batch, &["\\Deleted".to_owned()], &[])
                .await?;
            if self.uid_plus {
                let _: Vec<u32> = self
                    .session
                    .uid_expunge(uid_set(batch))
                    .await?
                    .try_collect()
                    .await?;
            } else {
                // Plain `EXPUNGE` takes every message marked deleted, the batch's or not.
                let _: Vec<u32> = self.session.expunge().await?.try_collect().await?;
            }
        }
        Ok(())
    }

    async fn append(&mut self, folder: &str, flags: &[String], raw: &[u8]) -> Result<()> {
        let flags = (!flags.is_empty()).then(|| format!("({})", flags.join(" ")));
        Ok(self
            .session
            .append(folder, flags.as_deref(), None, raw)
            .await?)
    }

    async fn submit(&mut self, sender: &str, recipients: &[String], raw: &[u8]) -> Result<()> {
        let smtp = self.smtp.as_ref().context("no outgoing mail server")?;
        let address = |text: &str| {
            text.parse::<Address>()
                .with_context(|| format!("{text:?} is not an address"))
        };
        let envelope = Envelope::new(
            Some(address(sender)?),
            recipients
                .iter()
                .map(|to| address(to))
                .collect::<Result<_>>()?,
        )?;
        smtp.send_raw(&envelope, raw).await?;
        Ok(())
    }
}
