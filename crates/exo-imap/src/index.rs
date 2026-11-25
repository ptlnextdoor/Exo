//! The facade's memory across restarts: which IMAP message each Gmail ID stands for, and the
//! history log. Without it every launch would hand Exo new IDs and make it rebuild its cache.

use std::path::Path;

use anyhow::Result;
use gmail_mock::engine::{Change, Record};
use rusqlite::{Connection, OptionalExtension, params};

use crate::labels::Role;
use crate::mailstore::FolderState;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS folders (
    position INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    role TEXT NOT NULL,
    -- The user label of a folder with no role. Labels are created in `position` order, which
    -- keeps their `Label_N` IDs the same from one launch to the next.
    label TEXT,
    uid_validity INTEGER,
    uid_next INTEGER,
    highest_modseq INTEGER
);
CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    folder TEXT NOT NULL,
    uid INTEGER NOT NULL,
    uid_validity INTEGER NOT NULL,
    content_key BLOB NOT NULL,
    internal_date INTEGER NOT NULL,
    history_id INTEGER NOT NULL,
    flags TEXT NOT NULL,
    header BLOB NOT NULL,
    UNIQUE (folder, uid_validity, uid)
);
CREATE INDEX IF NOT EXISTS messages_by_key ON messages (content_key);
CREATE TABLE IF NOT EXISTS history (
    id INTEGER PRIMARY KEY,
    message_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    labels TEXT NOT NULL,
    kind TEXT NOT NULL,
    changed TEXT NOT NULL
);
";

pub struct Index {
    db: Connection,
}

#[derive(Debug, Clone)]
pub struct FolderRow {
    pub name: String,
    pub role: Role,
    pub label: Option<String>,
    pub state: Option<FolderState>,
}

/// A message as indexed.
#[derive(Debug, Clone)]
pub struct MessageRow {
    pub id: String,
    pub thread_id: String,
    pub folder: String,
    pub uid: u32,
    pub uid_validity: u32,
    pub content_key: Vec<u8>,
    pub internal_date: i64,
    pub history_id: u64,
    pub flags: Vec<String>,
    pub header: Vec<u8>,
}

impl Index {
    /// Opens (creating if needed) the index at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        Self::with(Connection::open(path)?)
    }

    pub fn in_memory() -> Result<Self> {
        Self::with(Connection::open_in_memory()?)
    }

    fn with(db: Connection) -> Result<Self> {
        db.execute_batch(SCHEMA)?;
        Ok(Self { db })
    }

    /// Groups the writes until [`Index::commit`], so a crash leaves the last pass whole.
    pub fn begin(&self) -> Result<()> {
        Ok(self.db.execute_batch("BEGIN")?)
    }

    pub fn commit(&self) -> Result<()> {
        Ok(self.db.execute_batch("COMMIT")?)
    }

    pub fn rollback(&self) {
        // Nothing to undo when the transaction never began.
        let _ = self.db.execute_batch("ROLLBACK");
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row("SELECT value FROM meta WHERE key = ?", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.db.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
        Ok(())
    }

    // Folders

    pub fn folders(&self) -> Result<Vec<FolderRow>> {
        let mut query = self.db.prepare(
            "SELECT name, role, label, uid_validity, uid_next, highest_modseq \
             FROM folders ORDER BY position",
        )?;
        let rows = query.query_map([], |r| {
            let validity: Option<u32> = r.get(3)?;
            let next: Option<u32> = r.get(4)?;
            let highest_modseq = r.get::<_, Option<i64>>(5)?.map(|m| m as u64);
            Ok(FolderRow {
                name: r.get(0)?,
                role: r.get::<_, String>(1)?.parse().unwrap_or(Role::Other),
                label: r.get(2)?,
                state: validity
                    .zip(next)
                    .map(|(uid_validity, uid_next)| FolderState {
                        uid_validity,
                        uid_next,
                        highest_modseq,
                    }),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_folder(&self, name: &str, role: Role, label: Option<&str>) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO folders (name, role, label) VALUES (?, ?, ?)",
            params![name, role.to_string(), label],
        )?;
        Ok(())
    }

    /// Records what the server said of `name` after a pass that indexed all of it.
    pub fn set_folder_state(&self, name: &str, state: &FolderState) -> Result<()> {
        self.db.execute(
            "UPDATE folders SET uid_validity = ?, uid_next = ?, highest_modseq = ? WHERE name = ?",
            params![
                state.uid_validity,
                state.uid_next,
                state.highest_modseq.map(|m| m as i64),
                name
            ],
        )?;
        Ok(())
    }

    // Messages

    /// Every message, for rebuilding the mailbox at launch.
    pub fn messages(&self) -> Result<Vec<MessageRow>> {
        self.query_messages("", [], "header")
    }

    /// A folder's messages without their headers, which a pass has no need of. Mail that is sent
    /// but not yet on the server is not among them.
    pub fn messages_in(&self, folder: &str) -> Result<Vec<MessageRow>> {
        self.query_messages("WHERE folder = ? AND uid_validity != 0", [folder], "x''")
    }

    /// Sent mail the server has no copy of yet. It has `uid_validity` 0, which no server uses,
    /// and the whole message in `header`.
    pub fn pending(&self) -> Result<Vec<MessageRow>> {
        self.query_messages("WHERE uid_validity = 0", [], "header")
    }

    pub fn message(&self, id: &str) -> Result<Option<MessageRow>> {
        Ok(self.query_messages("WHERE id = ?", [id], "header")?.pop())
    }

    fn query_messages<P: rusqlite::Params>(
        &self,
        filter: &str,
        params: P,
        header: &str,
    ) -> Result<Vec<MessageRow>> {
        let mut query = self.db.prepare(&format!(
            "SELECT id, thread_id, folder, uid, uid_validity, content_key, internal_date, \
             history_id, flags, {header} FROM messages {filter}"
        ))?;
        let rows = query.query_map(params, |r| {
            let flags: String = r.get(8)?;
            Ok(MessageRow {
                id: r.get(0)?,
                thread_id: r.get(1)?,
                folder: r.get(2)?,
                uid: r.get(3)?,
                uid_validity: r.get(4)?,
                content_key: r.get(5)?,
                internal_date: r.get(6)?,
                history_id: r.get::<_, i64>(7)? as u64,
                flags: serde_json::from_str(&flags).unwrap_or_default(),
                header: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn insert_message(&self, m: &MessageRow) -> Result<()> {
        self.db.execute(
            "INSERT INTO messages (id, thread_id, folder, uid, uid_validity, content_key, \
             internal_date, history_id, flags, header) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                m.id,
                m.thread_id,
                m.folder,
                m.uid,
                m.uid_validity,
                m.content_key,
                m.internal_date,
                m.history_id as i64,
                serde_json::to_string(&m.flags)?,
                m.header
            ],
        )?;
        Ok(())
    }

    /// A message that is the same one in another folder (a move, as far as IMAP can tell).
    pub fn relocate(
        &self,
        id: &str,
        folder: &str,
        uid: u32,
        uid_validity: u32,
        flags: &[String],
    ) -> Result<()> {
        self.db.execute(
            "UPDATE messages SET folder = ?, uid = ?, uid_validity = ?, flags = ? WHERE id = ?",
            params![folder, uid, uid_validity, serde_json::to_string(flags)?, id],
        )?;
        Ok(())
    }

    pub fn set_header(&self, id: &str, header: &[u8]) -> Result<()> {
        self.db.execute(
            "UPDATE messages SET header = ? WHERE id = ?",
            params![header, id],
        )?;
        Ok(())
    }

    pub fn set_flags(&self, id: &str, flags: &[String]) -> Result<()> {
        self.db.execute(
            "UPDATE messages SET flags = ? WHERE id = ?",
            params![serde_json::to_string(flags)?, id],
        )?;
        Ok(())
    }

    pub fn set_history_id(&self, id: &str, history_id: u64) -> Result<()> {
        self.db.execute(
            "UPDATE messages SET history_id = ? WHERE id = ?",
            params![history_id as i64, id],
        )?;
        Ok(())
    }

    pub fn delete_message(&self, id: &str) -> Result<()> {
        self.db.execute("DELETE FROM messages WHERE id = ?", [id])?;
        Ok(())
    }

    // History

    pub fn save_history(&self, records: &[Record]) -> Result<()> {
        let mut insert = self.db.prepare(
            "INSERT OR REPLACE INTO history (id, message_id, thread_id, labels, kind, changed) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )?;
        for record in records {
            let (kind, changed) = match &record.change {
                Change::MessageAdded => ("added", Vec::new()),
                Change::MessageDeleted => ("deleted", Vec::new()),
                Change::LabelsAdded(ids) => ("labels+", ids.clone()),
                Change::LabelsRemoved(ids) => ("labels-", ids.clone()),
            };
            insert.execute(params![
                record.id as i64,
                record.message_id,
                record.thread_id,
                serde_json::to_string(&record.labels)?,
                kind,
                serde_json::to_string(&changed)?
            ])?;
        }
        Ok(())
    }

    pub fn history(&self) -> Result<Vec<Record>> {
        let mut query = self.db.prepare(
            "SELECT id, message_id, thread_id, labels, kind, changed FROM history ORDER BY id",
        )?;
        let rows = query.query_map([], |r| {
            let ids = |text: String| serde_json::from_str::<Vec<String>>(&text).unwrap_or_default();
            let changed = ids(r.get(5)?);
            let change = match r.get::<_, String>(4)?.as_str() {
                "deleted" => Change::MessageDeleted,
                "labels+" => Change::LabelsAdded(changed),
                "labels-" => Change::LabelsRemoved(changed),
                _ => Change::MessageAdded,
            };
            Ok(Record {
                id: r.get::<_, i64>(0)? as u64,
                message_id: r.get(1)?,
                thread_id: r.get(2)?,
                labels: ids(r.get(3)?),
                change,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}
