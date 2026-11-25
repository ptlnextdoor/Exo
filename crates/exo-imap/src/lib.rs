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
//! This is the read-only half: mail arrives, flags and moves made elsewhere show up, and every
//! request that would change the mailbox is refused (403). Nothing is sent, moved or flagged on
//! the server.
//!
//! Known limits of this half:
//! - Lists hold headers only, so a row's snippet is empty until its message is opened. The text
//!   is fetched when a message, its thread or an attachment is read, and kept in memory.
//! - Drafts folders are not indexed, and Exo's hidden labels exist but only read from the
//!   `$ExoMuted`, `$ExoSnoozed` and `$ExoRU` keywords.
//! - Text search is Exo's own index; nothing asks the server to search.
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
