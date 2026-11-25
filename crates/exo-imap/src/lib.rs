#![deny(
    clippy::correctness,
    clippy::suspicious,
    clippy::complexity,
    clippy::perf
)]
//! Gmail's API over an IMAP mailbox, for `exo-store`.
//!
//! Exo's store speaks Gmail: threads, label IDs, history cursors. [`Facade`] keeps a
//! `gmail_mock` [`Engine`](gmail_mock::engine::Engine) in step with a [`Mailstore`] (IMAP, in
//! production) and [`router`] serves it, so the store runs unchanged against Purelymail or any
//! IMAP server.
//!
//! Mail arrives, and flags and moves made elsewhere show up. Exo's own changes reach the server
//! before the engine records them (see `facade::writes`):
//!
//! - Labels are folders and flags. `INBOX`, `SENT`, `TRASH`, `SPAM` and a label made from a folder
//!   each stand for a folder, and a message is in one folder, so a change that would put it in
//!   two is refused (400). A message with none of them is in Archive. `UNREAD`, `STARRED` and the
//!   hidden labels are `\Seen` (absent), `\Flagged` and the `$Exo*` keywords.
//! - `UID MOVE`, or `COPY`, `STORE` and `EXPUNGE` on a server without it. A folder the server
//!   lacks (Archive, Trash, Junk, Sent) is created.
//! - A send goes out through SMTP with `Bcc` removed, then is appended to Sent with `\Seen` and
//!   `Bcc` kept. It is in the index before the append, and the append is repeated after a crash
//!   unless Sent already holds a message with the same `Message-ID`, so it is sent once.
//! - A search with free text asks the server (`UID SEARCH TEXT`) in every folder and loads what it
//!   finds for the engine to match.
//! - If the server refuses a change, Exo gets 502 and nothing changed: its outbox retries.
//!
//! Known limits:
//! - Lists hold headers only, so a row's snippet is empty until its message is opened. The text
//!   is fetched when a message, its thread or an attachment is read, and kept in memory.
//! - Drafts live in the facade's memory only: they are not on the server, and a restart loses
//!   them. Drafts folders are not indexed.
//! - The hidden labels need the server to accept keywords (`PERMANENTFLAGS` with `\*`). One that
//!   does not makes mute and snooze fail with 502. `exo-imap --probe` shows what a server says.
//! - Labels cannot be made, renamed or deleted through Exo, and filters and settings are refused.
//! - Expunges are found by comparing UID sets, not with QRESYNC (`async-imap` does not parse
//!   `VANISHED`). A server with no CONDSTORE is read in full on every pass.
//! - Threading scans every message per insert, which is quadratic. A first sync of a mailbox
//!   with tens of thousands of messages will be slow.
//!
//! `async-imap` logs every command it sends, `LOGIN` with the password included. A program
//! that embeds this crate must keep the `async_imap` log target off (the `exo-imap` binary
//! does).

mod facade;
mod http;
pub mod imap;
mod index;
pub mod labels;
mod mailstore;

pub use facade::{Facade, Report};
pub use http::router;
pub use mailstore::{FolderInfo, FolderState, Header, Mailstore};
