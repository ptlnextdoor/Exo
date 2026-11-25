//! What the facade needs from a mail server. `ImapStore` is the real one; tests use a fake.

use anyhow::Result;

/// A folder as `LIST` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderInfo {
    pub name: String,
    pub delimiter: Option<String>,
    /// `LIST` attributes, e.g. `\Sent`, `\Noselect`.
    pub attributes: Vec<String>,
}

/// The selected folder's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderState {
    pub uid_validity: u32,
    pub uid_next: u32,
    /// `None` when the server has no CONDSTORE.
    pub highest_modseq: Option<u64>,
}

/// One message's headers and metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub uid: u32,
    /// IMAP flag names: `\Seen`, `\Flagged`, `$ExoMuted`...
    pub flags: Vec<String>,
    /// Epoch milliseconds.
    pub internal_date: i64,
    pub size: u32,
    /// `BODY[HEADER]`, including the blank line that ends it.
    pub header: Vec<u8>,
}

/// Reads of a mailbox. A call that needs a folder uses the one last `select`ed.
pub trait Mailstore: Send {
    fn folders(&mut self) -> impl Future<Output = Result<Vec<FolderInfo>>> + Send;

    fn select(&mut self, folder: &str) -> impl Future<Output = Result<FolderState>> + Send;

    /// Every UID in the selected folder.
    fn uids(&mut self) -> impl Future<Output = Result<Vec<u32>>> + Send;

    fn headers(&mut self, uids: &[u32]) -> impl Future<Output = Result<Vec<Header>>> + Send;

    /// The flags of messages changed after `modseq`, or of every message when `None`.
    fn flags(
        &mut self,
        changed_since: Option<u64>,
    ) -> impl Future<Output = Result<Vec<(u32, Vec<String>)>>> + Send;

    /// The whole message, without marking it read.
    fn body(&mut self, uid: u32) -> impl Future<Output = Result<Vec<u8>>> + Send;
}
