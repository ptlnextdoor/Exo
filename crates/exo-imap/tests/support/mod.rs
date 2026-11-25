//! A fake mail server, the facade served over HTTP, and a client for it.

#![allow(dead_code)] // Each test binary uses a different subset of these helpers.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use exo_imap::{Facade, FolderInfo, FolderState, Header, Mailstore};
use gmail::{Endpoints, Format, StaticToken};
use tokio::net::TcpListener;

pub const ACCOUNT: &str = "aayushya@kahlus.com";
pub const TOKEN: &str = "test-token";

pub struct Msg {
    pub raw: Vec<u8>,
    pub flags: Vec<String>,
    pub internal_date: i64,
    modseq: u64,
}

pub struct Folder {
    pub attributes: Vec<String>,
    pub uid_validity: u32,
    pub uid_next: u32,
    pub messages: BTreeMap<u32, Msg>,
}

/// The server's state, which a test changes between syncs as another mail client would.
pub struct Server {
    pub folders: BTreeMap<String, Folder>,
    modseq: u64,
    pub condstore: bool,
    /// Full-message fetches so far.
    pub body_fetches: usize,
    /// Fail every request, as a dropped connection does.
    pub down: bool,
    /// Fail every send, as an outgoing server that is down does.
    pub smtp_down: bool,
    /// What the outgoing server took: sender, recipients, message.
    pub outbox: Vec<(String, Vec<String>, Vec<u8>)>,
    /// What the server's own `SEARCH` calls were.
    pub searches: Vec<String>,
}

#[derive(Clone)]
pub struct Fake(pub Arc<Mutex<Server>>);

impl Fake {
    /// A server with the usual folders.
    pub fn new() -> Self {
        let folder = |attributes: &[&str]| Folder {
            attributes: attributes.iter().map(|a| (*a).to_owned()).collect(),
            uid_validity: 1,
            uid_next: 1,
            messages: BTreeMap::new(),
        };
        let folders = [
            ("INBOX", folder(&[])),
            ("Sent", folder(&["\\Sent"])),
            ("Drafts", folder(&["\\Drafts"])),
            ("Trash", folder(&["\\Trash"])),
            ("Archive", folder(&["\\Archive"])),
            ("Clients", folder(&[])),
        ]
        .into_iter()
        .map(|(name, folder)| (name.to_owned(), folder))
        .collect();
        Self(Arc::new(Mutex::new(Server {
            folders,
            modseq: 1,
            condstore: true,
            body_fetches: 0,
            down: false,
            smtp_down: false,
            outbox: Vec::new(),
            searches: Vec::new(),
        })))
    }

    pub fn server(&self) -> std::sync::MutexGuard<'_, Server> {
        self.0.lock().unwrap()
    }

    /// Delivers `raw` to `folder`. Returns its UID.
    pub fn deliver(&self, folder: &str, raw: Vec<u8>, flags: &[&str], internal_date: i64) -> u32 {
        let mut server = self.server();
        server.modseq += 1;
        let modseq = server.modseq;
        let folder = server.folders.get_mut(folder).unwrap();
        let uid = folder.uid_next;
        folder.uid_next += 1;
        folder.messages.insert(
            uid,
            Msg {
                raw,
                flags: flags.iter().map(|f| (*f).to_owned()).collect(),
                internal_date,
                modseq,
            },
        );
        uid
    }

    pub fn set_flags(&self, folder: &str, uid: u32, flags: &[&str]) {
        let mut server = self.server();
        server.modseq += 1;
        let modseq = server.modseq;
        let message = server
            .folders
            .get_mut(folder)
            .unwrap()
            .messages
            .get_mut(&uid)
            .unwrap();
        message.flags = flags.iter().map(|f| (*f).to_owned()).collect();
        message.modseq = modseq;
    }

    pub fn expunge(&self, folder: &str, uid: u32) {
        let mut server = self.server();
        server.modseq += 1;
        server
            .folders
            .get_mut(folder)
            .unwrap()
            .messages
            .remove(&uid);
    }

    /// Another client's `UID MOVE`: gone from `from`, new UID in `to`.
    pub fn mv(&self, from: &str, uid: u32, to: &str) -> u32 {
        let message = {
            let mut server = self.server();
            server.modseq += 1;
            server
                .folders
                .get_mut(from)
                .unwrap()
                .messages
                .remove(&uid)
                .unwrap()
        };
        let flags: Vec<&str> = message.flags.iter().map(String::as_str).collect();
        self.deliver(to, message.raw, &flags, message.internal_date)
    }

    /// The server lost track of a folder's UIDs and renumbered its mail.
    pub fn renumber(&self, folder: &str) {
        let mut server = self.server();
        server.modseq += 1;
        let folder = server.folders.get_mut(folder).unwrap();
        folder.uid_validity += 1;
        let old = std::mem::take(&mut folder.messages);
        folder.uid_next = 100;
        for (_, message) in old {
            folder.messages.insert(folder.uid_next, message);
            folder.uid_next += 1;
        }
    }

    pub fn store(&self) -> FakeStore {
        FakeStore {
            server: self.0.clone(),
            selected: None,
        }
    }
}

pub struct FakeStore {
    server: Arc<Mutex<Server>>,
    selected: Option<String>,
}

impl FakeStore {
    fn with<R>(&self, f: impl FnOnce(&mut Server, &str) -> Result<R>) -> Result<R> {
        let mut server = self.server.lock().unwrap();
        if server.down {
            return Err(anyhow!("connection lost"));
        }
        let selected = self
            .selected
            .clone()
            .ok_or_else(|| anyhow!("nothing selected"))?;
        f(&mut server, &selected)
    }
}

impl Mailstore for FakeStore {
    async fn folders(&mut self) -> Result<Vec<FolderInfo>> {
        let server = self.server.lock().unwrap();
        if server.down {
            return Err(anyhow!("connection lost"));
        }
        Ok(server
            .folders
            .iter()
            .map(|(name, folder)| FolderInfo {
                name: name.clone(),
                delimiter: Some("/".into()),
                attributes: folder.attributes.clone(),
            })
            .collect())
    }

    async fn select(&mut self, name: &str) -> Result<FolderState> {
        let server = self.server.lock().unwrap();
        if server.down {
            return Err(anyhow!("connection lost"));
        }
        let folder = server
            .folders
            .get(name)
            .ok_or_else(|| anyhow!("no such folder"))?;
        let state = FolderState {
            uid_validity: folder.uid_validity,
            uid_next: folder.uid_next,
            highest_modseq: server.condstore.then_some(server.modseq),
        };
        drop(server);
        self.selected = Some(name.to_owned());
        Ok(state)
    }

    async fn uids(&mut self) -> Result<Vec<u32>> {
        self.with(|server, folder| Ok(server.folders[folder].messages.keys().copied().collect()))
    }

    async fn headers(&mut self, uids: &[u32]) -> Result<Vec<Header>> {
        self.with(|server, folder| Ok(headers_of(&server.folders[folder], uids)))
    }

    async fn flags(&mut self, changed_since: Option<u64>) -> Result<Vec<(u32, Vec<String>)>> {
        self.with(|server, folder| {
            Ok(server.folders[folder]
                .messages
                .iter()
                .filter(|(_, m)| changed_since.is_none_or(|since| m.modseq > since))
                .map(|(uid, m)| (*uid, m.flags.clone()))
                .collect())
        })
    }

    async fn body(&mut self, uid: u32) -> Result<Vec<u8>> {
        self.with(|server, folder| {
            server.body_fetches += 1;
            server.folders[folder]
                .messages
                .get(&uid)
                .map(|m| m.raw.clone())
                .ok_or_else(|| anyhow!("gone"))
        })
    }

    async fn search(&mut self, query: &str) -> Result<Vec<u32>> {
        self.with(|server, folder| {
            server.searches.push(format!("{folder}: {query}"));
            // `HEADER Message-ID "x"` and `TEXT "x"`, the two the facade asks.
            let (field, needle) = match query.split_once(" \"") {
                Some((field, rest)) => (field, rest.trim_end_matches('"')),
                None => return Err(anyhow!("unsupported search {query}")),
            };
            let text = |raw: &[u8]| String::from_utf8_lossy(raw).to_lowercase();
            Ok(server.folders[folder]
                .messages
                .iter()
                .filter(|(_, m)| {
                    let text = text(&m.raw);
                    match field {
                        "HEADER Message-ID" => {
                            text.contains(&format!("<{}>", needle.to_lowercase()))
                        }
                        _ => text.contains(&needle.to_lowercase()),
                    }
                })
                .map(|(uid, _)| *uid)
                .collect())
        })
    }

    async fn create(&mut self, name: &str) -> Result<()> {
        let mut server = self.server.lock().unwrap();
        server.folders.entry(name.to_owned()).or_insert(Folder {
            attributes: Vec::new(),
            uid_validity: 1,
            uid_next: 1,
            messages: BTreeMap::new(),
        });
        Ok(())
    }

    async fn store_flags(&mut self, uids: &[u32], add: &[String], remove: &[String]) -> Result<()> {
        self.with(|server, folder| {
            server.modseq += 1;
            let modseq = server.modseq;
            for uid in uids {
                let message = server
                    .folders
                    .get_mut(folder)
                    .unwrap()
                    .messages
                    .get_mut(uid)
                    .ok_or_else(|| anyhow!("no message {uid}"))?;
                message.flags.retain(|f| !remove.contains(f));
                for flag in add {
                    if !message.flags.contains(flag) {
                        message.flags.push(flag.clone());
                    }
                }
                message.modseq = modseq;
            }
            Ok(())
        })
    }

    async fn move_to(&mut self, uids: &[u32], to: &str) -> Result<Vec<Header>> {
        let from = self
            .selected
            .clone()
            .ok_or_else(|| anyhow!("nothing selected"))?;
        let fake = Fake(self.server.clone());
        if fake.server().down {
            return Err(anyhow!("connection lost"));
        }
        let moved: Vec<u32> = uids.iter().map(|uid| fake.mv(&from, *uid, to)).collect();
        let server = self.server.lock().unwrap();
        Ok(headers_of(&server.folders[to], &moved))
    }

    async fn expunge(&mut self, uids: &[u32]) -> Result<()> {
        let selected = self
            .selected
            .clone()
            .ok_or_else(|| anyhow!("nothing selected"))?;
        if self.server.lock().unwrap().down {
            return Err(anyhow!("connection lost"));
        }
        for uid in uids {
            Fake(self.server.clone()).expunge(&selected, *uid);
        }
        Ok(())
    }

    async fn append(&mut self, folder: &str, flags: &[String], raw: &[u8]) -> Result<()> {
        if self.server.lock().unwrap().down {
            return Err(anyhow!("connection lost"));
        }
        let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
        Fake(self.server.clone()).deliver(folder, raw.to_vec(), &flags, 1_763_370_000_000);
        Ok(())
    }

    async fn submit(&mut self, sender: &str, recipients: &[String], raw: &[u8]) -> Result<()> {
        let mut server = self.server.lock().unwrap();
        if server.smtp_down {
            return Err(anyhow!("451 try again later"));
        }
        server
            .outbox
            .push((sender.to_owned(), recipients.to_vec(), raw.to_vec()));
        Ok(())
    }
}

fn headers_of(folder: &Folder, uids: &[u32]) -> Vec<Header> {
    uids.iter()
        .filter_map(|uid| {
            let message = folder.messages.get(uid)?;
            let end = message.raw.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
            Some(Header {
                uid: *uid,
                flags: message.flags.clone(),
                internal_date: message.internal_date,
                size: message.raw.len() as u32,
                header: message.raw[..end].to_vec(),
            })
        })
        .collect()
}

/// A plain-text message.
pub fn mail(from: &str, subject: &str, id: &str, reply_to: Option<&str>, body: &str) -> Vec<u8> {
    let threading = reply_to
        .map(|parent| format!("In-Reply-To: <{parent}>\r\nReferences: <{parent}>\r\n"))
        .unwrap_or_default();
    format!(
        "From: {from}\r\nTo: {ACCOUNT}\r\nSubject: {subject}\r\nMessage-ID: <{id}>\r\n\
         Date: Tue, 17 Nov 2026 09:00:00 +0000\r\n{threading}\
         Content-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n"
    )
    .into_bytes()
}

/// The facade serving at an address, and a Gmail client for it.
pub struct Served<S = FakeStore> {
    pub facade: Arc<Facade<S>>,
    pub gmail: gmail::Client,
    pub origin: String,
    task: tokio::task::JoinHandle<()>,
}

impl<S> Drop for Served<S> {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn serve<S: Mailstore + 'static>(facade: Arc<Facade<S>>) -> Served<S> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = exo_imap::router(facade.clone(), TOKEN);
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let gmail = gmail::Client::new(
        Endpoints::mock(&origin.parse().unwrap()),
        StaticToken(TOKEN.into()),
    );
    Served {
        facade,
        gmail,
        origin,
        task,
    }
}

pub fn open(fake: &Fake, index: Option<&std::path::Path>) -> Arc<Facade<FakeStore>> {
    Arc::new(Facade::open(fake.store(), index, ACCOUNT, "Aayushya").unwrap())
}

/// Every message of the mailbox as `subject => (id, thread, sorted labels)`.
pub async fn messages(client: &gmail::Client) -> BTreeMap<String, (String, String, Vec<String>)> {
    let api = client.gmail();
    let query = gmail::ListQuery {
        include_spam_trash: true,
        max_results: Some(500),
        ..gmail::ListQuery::default()
    };
    let mut all = BTreeMap::new();
    for item in api.messages(&query).await.unwrap().messages {
        let message = api
            .message(item.id.as_deref().unwrap(), Format::Metadata, &["Subject"])
            .await
            .unwrap();
        let subject = message
            .payload
            .as_ref()
            .and_then(|payload| payload.header("Subject"))
            .unwrap_or_default()
            .to_owned();
        let mut labels = message.label_ids.clone();
        labels.sort();
        all.insert(
            subject,
            (message.id.unwrap(), message.thread_id.unwrap(), labels),
        );
    }
    all
}

/// The history after `start`, as `(kind, message id)` pairs. `None` when it expired.
pub async fn history(client: &gmail::Client, start: u64) -> Option<Vec<(&'static str, String)>> {
    let response = client
        .gmail()
        .history(&gmail::HistoryQuery {
            start_history_id: start,
            ..gmail::HistoryQuery::default()
        })
        .await
        .ok()?;
    let mut changes = Vec::new();
    for record in response.history {
        let id = |m: &gmail::Message| m.id.clone().unwrap();
        changes.extend(
            record
                .messages_added
                .iter()
                .map(|c| ("added", id(&c.message))),
        );
        changes.extend(
            record
                .messages_deleted
                .iter()
                .map(|c| ("deleted", id(&c.message))),
        );
        changes.extend(
            record
                .labels_added
                .iter()
                .map(|c| ("labels+", id(&c.message))),
        );
        changes.extend(
            record
                .labels_removed
                .iter()
                .map(|c| ("labels-", id(&c.message))),
        );
    }
    Some(changes)
}

pub async fn history_id(client: &gmail::Client) -> u64 {
    client.gmail().profile().await.unwrap().history_id.unwrap()
}
