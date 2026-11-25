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

/// A mailbox and the way out of it. A call that needs a folder uses the one last `select`ed.
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

    /// Message UIDs of the selected folder that match an IMAP `SEARCH` query.
    fn search(&mut self, query: &str) -> impl Future<Output = Result<Vec<u32>>> + Send;

    fn create(&mut self, folder: &str) -> impl Future<Output = Result<()>> + Send;

    /// Adds and removes flags on messages of the selected folder.
    fn store_flags(
        &mut self,
        uids: &[u32],
        add: &[String],
        remove: &[String],
    ) -> impl Future<Output = Result<()>> + Send;

    /// Moves messages of the selected folder to `to`. Returns the headers of what arrived there,
    /// which are more than the moved messages when mail came in meanwhile.
    fn move_to(
        &mut self,
        uids: &[u32],
        to: &str,
    ) -> impl Future<Output = Result<Vec<Header>>> + Send;

    /// Removes messages of the selected folder for good.
    fn expunge(&mut self, uids: &[u32]) -> impl Future<Output = Result<()>> + Send;

    fn append(
        &mut self,
        folder: &str,
        flags: &[String],
        raw: &[u8],
    ) -> impl Future<Output = Result<()>> + Send;

    /// Hands `raw` to the outgoing server for `recipients`, from `sender`.
    fn submit(
        &mut self,
        sender: &str,
        recipients: &[String],
        raw: &[u8],
    ) -> impl Future<Output = Result<()>> + Send;
}
