//! Writes: a change reaches the server first, and the engine records it only once the server has
//! it. If the server refuses, Exo gets an error and its outbox retries.
//!
//! The index is updated last of all. A write that fails half way therefore leaves the index as
//! the last pass saw the server, and the next pass finds what did happen the way it finds any
//! change made by another client.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Context, Result};
use gmail_mock::engine::Engine;
use tokio::sync::MutexGuard;

use super::{State, content_key};
use crate::index::MessageRow;
use crate::labels::{self, Role};
use crate::mailstore::{Header, Mailstore};

/// Why a write did not happen.
#[derive(Debug)]
pub enum Failed {
    /// Exo asked for something the mailbox cannot be.
    Request(String),
    /// The mail server did not do it.
    Server(anyhow::Error),
}

impl From<anyhow::Error> for Failed {
    fn from(error: anyhow::Error) -> Self {
        Self::Server(error)
    }
}

/// What the server did with a message, to be written to the index once the engine agrees.
pub struct Done {
    id: String,
    flags: Vec<String>,
    /// The new folder, UID and UIDVALIDITY of a message that moved.
    moved: Option<(String, u32, u32)>,
    /// Gone from the server.
    deleted: bool,
}

/// A draft as it was before it was sent, to put back if the send fails.
pub struct Draft {
    pub id: String,
    pub raw: Vec<u8>,
    pub thread_id: Option<String>,
}

/// The facade, held for one write.
pub struct Writer<'a, S> {
    state: MutexGuard<'a, State<S>>,
    engine: &'a Engine,
}

/// A folder, and the flags to add and remove in it.
type FlagChange = (String, Vec<String>, Vec<String>);

/// Where a message should be.
enum Want {
    Role(Role),
    Folder(String),
}

impl<'a, S: Mailstore> Writer<'a, S> {
    pub(super) fn new(state: MutexGuard<'a, State<S>>, engine: &'a Engine) -> Self {
        Self { state, engine }
    }

    /// Sets `ids` to carry the labels that `add` and `remove` change, in the flags and folders
    /// of the server. Call [`commit`](Self::commit) once the engine has the same change.
    pub async fn labels(
        &mut self,
        ids: &[String],
        add: &[String],
        remove: &[String],
    ) -> Result<Vec<Done>, Failed> {
        // Messages that get the same flag change, and those that go the same way.
        let mut flag_groups: HashMap<FlagChange, Vec<MessageRow>> = HashMap::new();
        let mut moves: HashMap<(String, String), Vec<MessageRow>> = HashMap::new();
        let mut done = Vec::new();
        for id in ids {
            let Some(row) = self.state.index.message(id)? else {
                continue;
            };
            if row.uid_validity == 0 {
                return Err(Failed::Request("That message is still being sent.".into()));
            }
            let Some(mut labels) = self
                .engine
                .with(|mailbox| mailbox.message(id).ok().map(|m| m.labels.clone()))
            else {
                continue;
            };
            labels.retain(|label| !remove.contains(label) || add.contains(label));
            labels.extend(add.iter().cloned());

            let (flags_add, flags_remove, flags) = flag_changes(&row, &labels, &self.state.hidden);
            let want = self.want(&labels)?;
            let folders = self.state.index.folders()?;
            let here = folders.iter().find(|f| f.name == row.folder);
            let stays = match &want {
                Want::Role(role) => here.is_some_and(|f| f.role == *role),
                Want::Folder(name) => *name == row.folder,
            };
            if !flags_add.is_empty() || !flags_remove.is_empty() {
                flag_groups
                    .entry((row.folder.clone(), flags_add, flags_remove))
                    .or_default()
                    .push(row.clone());
            }
            if !stays {
                let to = self.resolve(want).await?;
                moves
                    .entry((row.folder.clone(), to))
                    .or_default()
                    .push(row.clone());
            }
            // Every message gets an entry: the engine's change moves its history position even
            // when the server has nothing to do.
            done.push(Done {
                id: row.id,
                flags,
                moved: None,
                deleted: false,
            });
        }

        for ((folder, add, remove), rows) in flag_groups {
            let uids = self.select_rows(&folder, rows.iter()).await?;
            self.state.store.store_flags(&uids, &add, &remove).await?;
        }
        for ((from, to), rows) in moves {
            let uids = self.select_rows(&from, rows.iter()).await?;
            let validity = self.state.store.select(&to).await?.uid_validity;
            self.state.store.select(&from).await?;
            let mut arrived = self.state.store.move_to(&uids, &to).await?;
            for row in rows {
                let at = arrived
                    .iter()
                    .position(|h| content_key(h) == row.content_key);
                let Some(header) = at.map(|at| arrived.swap_remove(at)) else {
                    // The next pass finds the message where it went.
                    log::warn!("{} moved to {to}, but its new place is not known", row.id);
                    continue;
                };
                if let Some(change) = done.iter_mut().find(|d| d.id == row.id) {
                    if !header.flags.is_empty() {
                        change.flags = header.flags;
                    }
                    change.moved = Some((to.clone(), header.uid, validity));
                }
            }
        }
        Ok(done)
    }

    /// Removes `ids` from the server for good.
    pub async fn delete(&mut self, ids: &[String]) -> Result<Vec<Done>, Failed> {
        let mut by_folder: HashMap<String, Vec<MessageRow>> = HashMap::new();
        for id in ids {
            if let Some(row) = self.state.index.message(id)? {
                by_folder.entry(row.folder.clone()).or_default().push(row);
            }
        }
        let mut done = Vec::new();
        for (folder, rows) in by_folder {
            let (sent, kept): (Vec<_>, Vec<_>) =
                rows.into_iter().partition(|r| r.uid_validity == 0);
            // Mail that is only in the index has nothing to remove from the server.
            done.extend(sent.into_iter().map(|row| gone(row.id)));
            if kept.is_empty() {
                continue;
            }
            let uids = self.select_rows(&folder, kept.iter()).await?;
            self.state.store.expunge(&uids).await?;
            done.extend(kept.into_iter().map(|row| gone(row.id)));
        }
        Ok(done)
    }

    /// Writes what the server did to the index, with the history the engine made of it.
    pub fn commit(&mut self, done: Vec<Done>) -> Result<()> {
        self.state.index.begin()?;
        let saved = self.engine.with(|mailbox| -> Result<()> {
            for change in &done {
                if change.deleted {
                    self.state.index.delete_message(&change.id)?;
                    continue;
                }
                match &change.moved {
                    Some((folder, uid, validity)) => self.state.index.relocate(
                        &change.id,
                        folder,
                        *uid,
                        *validity,
                        &change.flags,
                    )?,
                    None => self.state.index.set_flags(&change.id, &change.flags)?,
                }
                if let Ok(message) = mailbox.message(&change.id) {
                    self.state
                        .index
                        .set_history_id(&change.id, message.history_id)?;
                }
            }
            self.state.persist_history(mailbox, false)
        });
        match saved {
            Ok(()) => Ok(self.state.index.commit()?),
            Err(error) => {
                self.state.index.rollback();
                Err(error)
            }
        }
    }

    /// Sends the message the engine has just made `id`: through the outgoing server, then into
    /// Sent. If the outgoing server refuses, the message (and `draft`) is as it was before.
    pub async fn sent(&mut self, id: &str, draft: Option<Draft>) -> Result<(), Failed> {
        let sent = self.state.folder_of(self.engine, Role::Sent).await;
        let outcome = async {
            let sent = sent?;
            let (raw, recipients, sender) = self
                .engine
                .with(|mailbox| {
                    mailbox.message(id).map(|m| {
                        (
                            m.raw.clone(),
                            m.mime.recipients.clone(),
                            mailbox.email.clone(),
                        )
                    })
                })
                .map_err(|f| anyhow::anyhow!("{:?}", f.error))?;
            let mut seen = BTreeSet::new();
            let recipients: Vec<String> = recipients
                .into_iter()
                .filter(|to| seen.insert(to.to_ascii_lowercase()))
                .collect();
            self.state
                .store
                .submit(&sender, &recipients, &without_bcc(&raw))
                .await?;
            Ok::<_, anyhow::Error>((sent, raw))
        }
        .await;
        let (sent, raw) = match outcome {
            Ok(sent) => sent,
            Err(error) => {
                let now = jiff::Timestamp::now().as_millisecond();
                self.engine.with(|mailbox| {
                    let _ = mailbox.delete(id);
                    if let Some(draft) = draft {
                        let _ = mailbox.restore_draft(&draft.id, draft.raw, draft.thread_id, now);
                    }
                });
                return Err(Failed::Server(error));
            }
        };

        // The server has the mail. Record it now, before anything else can fail, so that nothing
        // sends it twice, and take INBOX off the copy, which the server delivers to itself.
        let end = header_end(&raw);
        let header = Header {
            uid: 0,
            flags: vec!["\\Seen".into()],
            internal_date: 0,
            size: raw.len() as u32,
            header: raw[..end].to_vec(),
        };
        self.state.index.begin()?;
        let saved = self.engine.with(|mailbox| -> Result<()> {
            mailbox.apply_labels(id, &[], &["INBOX".into()]);
            let message = mailbox
                .message(id)
                .map_err(|f| anyhow::anyhow!("{:?}", f.error))?;
            // No server uses UIDVALIDITY 0, and the UID only has to differ from other sent mail's.
            let uid =
                (u64::from_str_radix(&message.id, 16).unwrap_or_default() & 0xFFFF_FFFF) as u32;
            self.state.index.insert_message(&MessageRow {
                id: message.id.clone(),
                thread_id: message.thread_id.clone(),
                folder: sent,
                uid,
                uid_validity: 0,
                content_key: content_key(&header),
                internal_date: message.internal_date,
                history_id: message.history_id,
                flags: header.flags.clone(),
                header: raw.clone(),
            })?;
            self.state.persist_history(mailbox, false)
        });
        match saved {
            Ok(()) => self.state.index.commit()?,
            Err(error) => {
                self.state.index.rollback();
                return Err(error.into());
            }
        }
        if let Err(error) = self.state.settle_pending().await {
            log::warn!("sent, but not yet filed in Sent: {error:#}");
        }
        Ok(())
    }

    /// The folder (and its UIDs) of `rows`, selected, provided the server still numbers it as the
    /// index does.
    async fn select_rows<'r>(
        &mut self,
        folder: &str,
        rows: impl Iterator<Item = &'r MessageRow>,
    ) -> Result<Vec<u32>, Failed> {
        let state = self.state.store.select(folder).await?;
        let rows: Vec<_> = rows.collect();
        if rows
            .iter()
            .any(|row| row.uid_validity != state.uid_validity)
        {
            return Err(Failed::Server(anyhow::anyhow!(
                "{folder} changed on the server; waiting for the next sync"
            )));
        }
        Ok(rows.iter().map(|row| row.uid).collect())
    }

    /// The folder a message with `labels` belongs in.
    fn want(&self, labels: &BTreeSet<String>) -> Result<Want, Failed> {
        if labels.contains("TRASH") {
            return Ok(Want::Role(Role::Trash));
        }
        if labels.contains("SPAM") {
            return Ok(Want::Role(Role::Junk));
        }
        let mut wants = Vec::new();
        if labels.contains("INBOX") {
            wants.push(Want::Role(Role::Inbox));
        }
        if labels.contains("SENT") {
            wants.push(Want::Role(Role::Sent));
        }
        for folder in self.state.index.folders()? {
            let id = folder
                .label
                .as_ref()
                .and_then(|label| self.state.user_labels.get(label));
            if id.is_some_and(|id| labels.contains(id)) {
                wants.push(Want::Folder(folder.name));
            }
        }
        match wants.len() {
            0 => Ok(Want::Role(Role::Archive)),
            1 => Ok(wants.remove(0)),
            _ => Err(Failed::Request(
                "A message lives in one folder, so it takes one of Inbox, Sent and your labels."
                    .into(),
            )),
        }
    }

    /// The name of the folder to move to, creating a standard one the server lacks.
    async fn resolve(&mut self, want: Want) -> Result<String, Failed> {
        Ok(match want {
            Want::Folder(name) => name,
            Want::Role(role) => self.state.folder_of(self.engine, role).await?,
        })
    }
}

impl<S: Mailstore> State<S> {
    /// The folder with `role`, which is created if the server has none.
    async fn folder_of(&mut self, engine: &Engine, role: Role) -> Result<String> {
        let find = |state: &Self| -> Result<Option<String>> {
            Ok(state
                .index
                .folders()?
                .into_iter()
                .find(|f| f.role == role)
                .map(|f| f.name))
        };
        if let Some(name) = find(self)? {
            return Ok(name);
        }
        let name = match role {
            Role::Archive => "Archive",
            Role::Trash => "Trash",
            Role::Junk => "Junk",
            Role::Sent => "Sent",
            other => anyhow::bail!("the server has no {other} folder"),
        };
        self.store.create(name).await?;
        let listed = self.store.folders().await?;
        self.register(engine, &listed)?;
        find(self)?.with_context(|| format!("the server did not make a {name} folder"))
    }

    /// Files sent mail in Sent: for each message the server does not hold yet, `APPEND` it
    /// (unless the server saved a copy itself) and learn its UID.
    pub(super) async fn settle_pending(&mut self) -> Result<()> {
        for row in self.index.pending()? {
            let state = self.store.select(&row.folder).await?;
            let query = match message_id(&row.header) {
                Some(id) => format!("HEADER Message-ID \"{}\"", id.replace(['"', '\\'], "")),
                None => anyhow::bail!("{} has no Message-ID", row.id),
            };
            let mut uids = self.store.search(&query).await?;
            if uids.is_empty() {
                self.store
                    .append(&row.folder, &["\\Seen".into()], &row.header)
                    .await?;
                uids = self.store.search(&query).await?;
            }
            let Some(uid) = uids.last().copied() else {
                anyhow::bail!("{} is not in {} after it was appended", row.id, row.folder);
            };
            let found = self.store.headers(&[uid]).await?;
            let header = found.first().context("the appended message vanished")?;
            self.index.begin()?;
            let moved = self
                .index
                .relocate(&row.id, &row.folder, uid, state.uid_validity, &header.flags)
                .and_then(|()| self.index.set_header(&row.id, &header.header));
            match moved {
                Ok(()) => self.index.commit()?,
                Err(error) => {
                    self.index.rollback();
                    return Err(error);
                }
            }
        }
        Ok(())
    }
}

fn gone(id: String) -> Done {
    Done {
        id,
        flags: Vec::new(),
        moved: None,
        deleted: true,
    }
}

/// The flags to add and remove so that `row` has those `labels` stand for, and all its flags
/// after that.
fn flag_changes(
    row: &MessageRow,
    labels: &BTreeSet<String>,
    hidden: &[String],
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let has = |flags: &[String], flag: &str| flags.iter().any(|f| f.eq_ignore_ascii_case(flag));
    let mut add = Vec::new();
    let mut remove = Vec::new();
    let mut flags = row.flags.clone();
    for (flag, wanted) in labels::wanted_flags(labels, hidden) {
        match (wanted, has(&row.flags, flag)) {
            (true, false) => {
                add.push(flag.to_owned());
                flags.push(flag.to_owned());
            }
            (false, true) => {
                remove.push(flag.to_owned());
                flags.retain(|f| !f.eq_ignore_ascii_case(flag));
            }
            _ => {}
        }
    }
    (add, remove, flags)
}

/// Where the header of `raw` ends, after its blank line.
pub(crate) fn header_end(raw: &[u8]) -> usize {
    raw.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(raw.len(), |at| at + 4)
}

/// `raw` without its `Bcc` header: what the recipients get. The copy in Sent keeps it.
pub(crate) fn without_bcc(raw: &[u8]) -> Vec<u8> {
    let end = header_end(raw);
    let mut out = Vec::with_capacity(raw.len());
    let mut skipping = false;
    for line in raw[..end].split_inclusive(|b| *b == b'\n') {
        let folded = line.starts_with(b" ") || line.starts_with(b"\t");
        if !folded {
            skipping = line.len() >= 4 && line[..4].eq_ignore_ascii_case(b"bcc:");
        }
        if !skipping {
            out.extend_from_slice(line);
        }
    }
    out.extend_from_slice(&raw[end..]);
    out
}

/// The `Message-ID` of a message (without its angle brackets).
fn message_id(raw: &[u8]) -> Option<String> {
    let header = &raw[..header_end(raw)];
    let text = String::from_utf8_lossy(header);
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("Message-ID")
            .then(|| value.trim().trim_matches(['<', '>']).to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::{message_id, without_bcc};

    #[test]
    fn bcc_goes_with_its_folded_lines_and_nothing_else() {
        let raw = b"To: a@x\r\nBcc: b@x,\r\n c@x\r\nSubject: Bcc: not a header\r\nbcc: d@x\r\n\r\nBcc: body\r\n";
        assert_eq!(
            without_bcc(raw),
            b"To: a@x\r\nSubject: Bcc: not a header\r\n\r\nBcc: body\r\n"
        );
    }

    #[test]
    fn message_id_is_read_without_its_brackets() {
        assert_eq!(
            message_id(b"message-id: <a@b>\r\n\r\n").as_deref(),
            Some("a@b")
        );
        assert_eq!(
            message_id(b"Subject: x\r\n\r\nMessage-ID: <no@x>\r\n"),
            None
        );
    }
}
