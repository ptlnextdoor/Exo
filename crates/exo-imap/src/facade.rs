//! Keeps a Gmail-shaped [`Engine`] in step with a [`Mailstore`], through the [`Index`].
//!
//! A pass has two halves. The first asks the server what changed and touches nothing local, so a
//! failed connection leaves engine and index as they were. The second applies the answer to both
//! at once with no waiting in between.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use gmail::{Label, LabelListVisibility, MessageListVisibility};
use gmail_mock::engine::{Engine, HistoryLog, Mailbox, NewMessage, Restored, Threading};
use jiff::tz::TimeZone;
use sha2::{Digest, Sha256};

use crate::index::{FolderRow, Index, MessageRow};
use crate::labels::{self, HIDDEN, Role};
use crate::mailstore::{FolderInfo, FolderState, Header, Mailstore};

mod search;
mod writes;
pub use writes::{Draft, Failed, Writer};

/// Where a new mailbox's history starts.
const HISTORY_START: u64 = 1_000;

/// Passes closer together than this are one pass, so Exo's polling can't hammer the server.
const MIN_INTERVAL: Duration = Duration::from_secs(2);

/// What a pass changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub added: usize,
    pub deleted: usize,
    pub moved: usize,
    pub flagged: usize,
}

impl Report {
    pub fn changed(&self) -> bool {
        *self != Self::default()
    }
}

pub struct Facade<S> {
    pub engine: Engine,
    state: tokio::sync::Mutex<State<S>>,
    /// Messages whose full text the engine holds, as opposed to their headers.
    loaded: Mutex<HashSet<String>>,
}

struct State<S> {
    store: S,
    index: Index,
    /// IDs of [`HIDDEN`].
    hidden: Vec<String>,
    /// Label name to its ID, for folders that have one.
    user_labels: HashMap<String, String>,
    synced: Option<Instant>,
}

/// What a pass found in one folder.
struct Found {
    folder: String,
    state: Option<FolderState>,
    fresh: Vec<Header>,
    /// Indexed messages the server no longer has in this folder.
    gone: Vec<MessageRow>,
    flags: Vec<(MessageRow, Vec<String>)>,
}

fn content_key(header: &Header) -> Vec<u8> {
    let mut hash = Sha256::new();
    hash.update(header.size.to_le_bytes());
    hash.update(&header.header);
    hash.finalize().to_vec()
}

fn same_flags(a: &[String], b: &[String]) -> bool {
    a.iter().collect::<BTreeSet<_>>() == b.iter().collect::<BTreeSet<_>>()
}

/// A label for the mailbox's own use, hidden from Exo's lists when `hidden`.
fn create_label(mailbox: &mut Mailbox, name: &str, hidden: bool) -> Option<String> {
    mailbox
        .create_label(Label {
            name: Some(name.to_owned()),
            label_list_visibility: hidden.then_some(LabelListVisibility::LabelHide),
            message_list_visibility: hidden.then_some(MessageListVisibility::Hide),
            ..Label::default()
        })
        .ok()
        .and_then(|label| label.id)
}

impl<S: Mailstore> Facade<S> {
    /// Builds the mailbox from the index at `index_path` (no network), creating the index on a
    /// first run.
    pub fn open(store: S, index_path: Option<&Path>, email: &str, name: &str) -> Result<Self> {
        let index = match index_path {
            Some(path) => Index::open(path)?,
            None => Index::in_memory()?,
        };
        let number = |key: &str| {
            index
                .meta(key)
                .map(|v| v.and_then(|v| v.parse::<u64>().ok()))
        };
        let floor = number("history_floor")?.unwrap_or(HISTORY_START);
        let current = number("history_current")?.unwrap_or(floor);

        let mut mailbox = Mailbox::new(email, name, TimeZone::system(), floor);
        mailbox.settings.send_as = vec![gmail::SendAs {
            send_as_email: Some(email.to_owned()),
            display_name: Some(name.to_owned()),
            reply_to_address: Some(String::new()),
            is_primary: Some(true),
            is_default: Some(true),
            ..gmail::SendAs::default()
        }];
        let hidden: Vec<String> = HIDDEN
            .iter()
            .map(|name| create_label(&mut mailbox, name, true).unwrap_or_default())
            .collect();
        let folders = index.folders()?;
        let mut user_labels = HashMap::new();
        for label in folders.iter().filter_map(|folder| folder.label.as_deref()) {
            if let Some(id) = create_label(&mut mailbox, label, false) {
                user_labels.insert(label.to_owned(), id);
            }
        }
        let folder_labels = folder_labels(&folders, &user_labels);
        for row in index.messages()? {
            let labels = labels::message_labels(
                folder_labels
                    .get(&row.folder)
                    .map_or(&[][..], Vec::as_slice),
                &row.flags,
                &hidden,
            );
            let restored = mailbox.restore(Restored {
                id: row.id.clone(),
                thread_id: row.thread_id,
                labels,
                internal_date: row.internal_date,
                history_id: row.history_id,
                raw: row.header,
            });
            if let Err(error) = restored {
                log::warn!("dropping unreadable indexed message {}: {error:?}", row.id);
            }
        }
        mailbox.history = HistoryLog::from_parts(floor, current, index.history()?);

        Ok(Self {
            engine: Engine::new(mailbox),
            state: tokio::sync::Mutex::new(State {
                store,
                index,
                hidden,
                user_labels,
                synced: None,
            }),
            loaded: Mutex::new(HashSet::new()),
        })
    }

    /// Brings the mailbox up to date with the server.
    pub async fn sync(&self) -> Result<Report> {
        let mut state = self.state.lock().await;
        let report = state.pass(&self.engine).await?;
        state.synced = Some(Instant::now());
        Ok(report)
    }

    /// [`sync`](Self::sync), unless a pass just ran.
    pub async fn sync_if_stale(&self) -> Result<Report> {
        let mut state = self.state.lock().await;
        if state.synced.is_some_and(|at| at.elapsed() < MIN_INTERVAL) {
            return Ok(Report::default());
        }
        let report = state.pass(&self.engine).await?;
        state.synced = Some(Instant::now());
        Ok(report)
    }

    /// Takes the facade for a write: no sync or other write runs until the [`Writer`] is dropped.
    pub async fn writer(&self) -> Writer<'_, S> {
        Writer::new(self.state.lock().await, &self.engine)
    }

    /// Makes the engine hold the full text of `ids`, fetching what it only has headers of.
    pub async fn load_bodies(&self, ids: &[String]) -> Result<()> {
        let missing: Vec<&String> = {
            let loaded = self.loaded.lock().unwrap_or_else(|e| e.into_inner());
            ids.iter().filter(|id| !loaded.contains(*id)).collect()
        };
        if missing.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        for id in missing {
            let Some(row) = state.index.message(id)? else {
                continue;
            };
            if row.uid_validity == 0 {
                continue; // Sent and not yet on the server: the engine has all of it.
            }
            let folder = state.store.select(&row.folder).await?;
            if folder.uid_validity != row.uid_validity {
                bail!(
                    "{} changed on the server; waiting for the next sync",
                    row.folder
                );
            }
            let raw = state.store.body(row.uid).await?;
            self.engine
                .with(|mailbox| mailbox.replace_raw(id, raw))
                .map_err(|failure| anyhow::anyhow!("{:?}", failure.error))?;
            self.loaded
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id.clone());
        }
        Ok(())
    }
}

/// Folder name to the labels its messages carry, for the folders that have messages.
fn folder_labels(
    folders: &[FolderRow],
    user_labels: &HashMap<String, String>,
) -> HashMap<String, Vec<String>> {
    folders
        .iter()
        .map(|folder| {
            let mut labels: Vec<String> = folder
                .role
                .labels()
                .iter()
                .map(|l| (*l).to_owned())
                .collect();
            if let Some(id) = folder
                .label
                .as_ref()
                .and_then(|label| user_labels.get(label))
            {
                labels.push(id.clone());
            }
            (folder.name.clone(), labels)
        })
        .collect()
}

impl<S: Mailstore> State<S> {
    /// Saves the history the mailbox made since the last save, inside the open transaction.
    fn persist_history(&self, mailbox: &mut Mailbox, first_pass: bool) -> Result<()> {
        if first_pass {
            // Nobody has seen the history of the first ingest, so it needs no keeping.
            mailbox.history.expire();
        } else {
            let persisted = self
                .index
                .meta("history_current")?
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(HISTORY_START);
            let fresh: Vec<_> = mailbox
                .history
                .since(persisted)
                .map(|records| records.cloned().collect())
                .unwrap_or_default();
            self.index.save_history(&fresh)?;
        }
        self.index
            .set_meta("history_floor", &mailbox.history.floor().to_string())?;
        self.index
            .set_meta("history_current", &mailbox.history.current().to_string())?;
        Ok(())
    }

    async fn pass(&mut self, engine: &Engine) -> Result<Report> {
        if let Err(error) = self.settle_pending().await {
            log::warn!("sent mail is still not on the server: {error:#}");
        }
        let listed = self.store.folders().await?;
        self.register(engine, &listed)?;
        let folders = self.index.folders()?;

        // First half: ask.
        let mut found = Vec::new();
        let mut order: Vec<&FolderRow> = folders.iter().collect();
        order.sort_by_key(|folder| match folder.role {
            Role::Inbox => 0,
            Role::Sent => 1,
            _ => 2,
        });
        for folder in order {
            let listed = listed.iter().any(|info| info.name == folder.name);
            found.push(if listed {
                self.inspect(folder).await?
            } else {
                // The folder is gone from the server, and so are its messages.
                Found {
                    folder: folder.name.clone(),
                    state: None,
                    fresh: Vec::new(),
                    gone: self.index.messages_in(&folder.name)?,
                    flags: Vec::new(),
                }
            });
        }

        // Second half: apply.
        self.apply(engine, &folders, found)
    }

    /// Adds folders the index has not seen, with the label they get.
    fn register(&mut self, engine: &Engine, listed: &[FolderInfo]) -> Result<()> {
        let known: HashSet<String> = self.index.folders()?.into_iter().map(|f| f.name).collect();
        for info in listed {
            let role = Role::of(info);
            let selectable = !info
                .attributes
                .iter()
                .any(|a| a.eq_ignore_ascii_case("\\Noselect"));
            if known.contains(&info.name) || !selectable || role == Role::Drafts {
                continue;
            }
            // Drafts stay local until the app can send; the server's would only confuse Exo's own.
            let label = (role == Role::Other).then(|| labels::label_name(info));
            if let Some(label) = &label
                && let Some(id) = engine.with(|mailbox| create_label(mailbox, label, false))
            {
                self.user_labels.insert(label.clone(), id);
            }
            self.index.add_folder(&info.name, role, label.as_deref())?;
        }
        Ok(())
    }

    async fn inspect(&mut self, folder: &FolderRow) -> Result<Found> {
        let state = self.store.select(&folder.name).await?;
        let mut found = Found {
            folder: folder.name.clone(),
            state: Some(state),
            fresh: Vec::new(),
            gone: Vec::new(),
            flags: Vec::new(),
        };
        // The server says nothing changed since a pass that indexed everything.
        if state.highest_modseq.is_some() && folder.state == Some(state) {
            return Ok(found);
        }
        let known = self.index.messages_in(&folder.name)?;
        let reset = folder
            .state
            .is_some_and(|s| s.uid_validity != state.uid_validity);
        let on_server: HashSet<u32> = self.store.uids().await?.into_iter().collect();
        let (kept, gone): (Vec<_>, Vec<_>) = known
            .into_iter()
            .partition(|m| !reset && on_server.contains(&m.uid));
        let indexed: HashSet<u32> = kept.iter().map(|m| m.uid).collect();
        let mut fresh: Vec<u32> = on_server.difference(&indexed).copied().collect();
        fresh.sort_unstable();
        found.fresh = self.store.headers(&fresh).await?;
        found.gone = gone;
        if !kept.is_empty() {
            let since = folder
                .state
                .filter(|s| !reset && s.highest_modseq.is_some())
                .and_then(|s| s.highest_modseq);
            let current: HashMap<u32, Vec<String>> =
                self.store.flags(since).await?.into_iter().collect();
            found.flags = kept
                .into_iter()
                .filter_map(|m| {
                    let flags = current.get(&m.uid)?;
                    (!same_flags(flags, &m.flags)).then(|| (m, flags.clone()))
                })
                .collect();
        }
        Ok(found)
    }

    fn apply(
        &mut self,
        engine: &Engine,
        folders: &[FolderRow],
        found: Vec<Found>,
    ) -> Result<Report> {
        let folder_labels = folder_labels(folders, &self.user_labels);
        let labels_of = |folder: &str, flags: &[String], hidden: &[String]| {
            labels::message_labels(
                folder_labels.get(folder).map_or(&[][..], Vec::as_slice),
                flags,
                hidden,
            )
        };
        let first_pass = self.index.meta("history_current")?.is_none();

        // Messages that left one folder and showed up in another in this pass were moved.
        let mut vanished: HashMap<Vec<u8>, Vec<MessageRow>> = HashMap::new();
        for row in found.iter().flat_map(|f| f.gone.iter().cloned()) {
            vanished
                .entry(row.content_key.clone())
                .or_default()
                .push(row);
        }
        let mut arrivals = Vec::new();
        let mut moves = Vec::new();
        for f in &found {
            let validity = f.state.map_or(0, |s| s.uid_validity);
            for header in &f.fresh {
                let key = content_key(header);
                match vanished.get_mut(&key).and_then(Vec::pop) {
                    Some(old) => moves.push((old, f.folder.clone(), validity, header.clone())),
                    None => arrivals.push((f.folder.clone(), validity, key, header.clone())),
                }
            }
        }
        // Parents before replies, so threads form.
        arrivals.sort_by_key(|(_, _, _, h)| (h.internal_date, h.uid));

        let mut report = Report::default();
        self.index.begin()?;
        let applied = engine.with(|mailbox| -> Result<()> {
            let hidden = &self.hidden;
            let touch = |mailbox: &Mailbox, id: &str, index: &Index| -> Result<()> {
                if let Ok(message) = mailbox.message(id) {
                    index.set_history_id(id, message.history_id)?;
                }
                Ok(())
            };
            for (old, folder, validity, header) in moves {
                let before = labels_of(&old.folder, &old.flags, hidden);
                let after = labels_of(&folder, &header.flags, hidden);
                let add: Vec<String> = after.difference(&before).cloned().collect();
                let remove: Vec<String> = before.difference(&after).cloned().collect();
                mailbox.apply_labels(&old.id, &add, &remove);
                self.index
                    .relocate(&old.id, &folder, header.uid, validity, &header.flags)?;
                touch(mailbox, &old.id, &self.index)?;
                report.moved += 1;
            }
            for (folder, validity, key, header) in arrivals {
                let labels = labels_of(&folder, &header.flags, hidden);
                let inserted = mailbox.insert(NewMessage {
                    raw: header.header.clone(),
                    labels: labels.into_iter().collect(),
                    threading: Threading::Headers,
                    internal_date: header.internal_date,
                    apply_filters: false,
                });
                let id = match inserted {
                    Ok(id) => id,
                    Err(failure) => {
                        log::warn!(
                            "skipping unreadable message {folder}/{}: {:?}",
                            header.uid,
                            failure.error
                        );
                        continue;
                    }
                };
                let message = mailbox
                    .message(&id)
                    .map_err(|f| anyhow::anyhow!("{:?}", f.error))?;
                self.index.insert_message(&MessageRow {
                    id,
                    thread_id: message.thread_id.clone(),
                    folder,
                    uid: header.uid,
                    uid_validity: validity,
                    content_key: key,
                    internal_date: header.internal_date,
                    history_id: message.history_id,
                    flags: header.flags,
                    header: header.header,
                })?;
                report.added += 1;
            }
            for f in &found {
                for (row, flags) in &f.flags {
                    let before = labels_of(&row.folder, &row.flags, hidden);
                    let after = labels_of(&row.folder, flags, hidden);
                    let add: Vec<String> = after.difference(&before).cloned().collect();
                    let remove: Vec<String> = before.difference(&after).cloned().collect();
                    mailbox.apply_labels(&row.id, &add, &remove);
                    self.index.set_flags(&row.id, flags)?;
                    touch(mailbox, &row.id, &self.index)?;
                    report.flagged += 1;
                }
            }
            for row in vanished.into_values().flatten() {
                let _ = mailbox.delete(&row.id);
                self.index.delete_message(&row.id)?;
                report.deleted += 1;
            }

            self.persist_history(mailbox, first_pass)?;
            for f in &found {
                if let Some(state) = &f.state {
                    self.index.set_folder_state(&f.folder, state)?;
                }
            }
            Ok(())
        });
        match applied {
            Ok(()) => self.index.commit()?,
            Err(error) => {
                self.index.rollback();
                return Err(error);
            }
        }
        Ok(report)
    }
}
