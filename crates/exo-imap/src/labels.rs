//! Folders and flags as Gmail labels.

use std::collections::BTreeSet;

use crate::mailstore::FolderInfo;

/// Exo's hidden labels, created up front so Exo finds them and has no need to create them
/// (this facade has no writes yet). Their IDs are `Label_1`... in this order.
pub const HIDDEN: [&str; 4] = ["[Exo]", "[Exo]/Muted", "[Exo]/Is Snoozed", "[Exo]/ru"];

/// IMAP keywords that stand for the hidden labels: `HIDDEN[1..]`, in order.
const KEYWORDS: [&str; 3] = ["$ExoMuted", "$ExoSnoozed", "$ExoRU"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Junk,
    Archive,
    Other,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl std::str::FromStr for Role {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        [
            Self::Inbox,
            Self::Sent,
            Self::Drafts,
            Self::Trash,
            Self::Junk,
            Self::Archive,
            Self::Other,
        ]
        .into_iter()
        .find(|role| role.to_string() == s)
        .ok_or(())
    }
}

impl Role {
    /// The folder's role: its special-use attribute, else its name.
    pub fn of(folder: &FolderInfo) -> Self {
        let special = |attribute: &str| {
            folder
                .attributes
                .iter()
                .any(|a| a.eq_ignore_ascii_case(attribute))
        };
        let leaf = folder
            .delimiter
            .as_deref()
            .and_then(|d| folder.name.rsplit(d).next())
            .unwrap_or(&folder.name)
            .to_lowercase();
        let named = |names: &[&str]| names.contains(&leaf.as_str());
        if folder.name.eq_ignore_ascii_case("INBOX") {
            Self::Inbox
        } else if special("\\Sent") || named(&["sent", "sent items", "sent mail", "sent messages"])
        {
            Self::Sent
        } else if special("\\Drafts") || named(&["drafts", "draft"]) {
            Self::Drafts
        } else if special("\\Trash")
            || named(&["trash", "deleted items", "deleted messages", "bin"])
        {
            Self::Trash
        } else if special("\\Junk") || named(&["junk", "spam", "junk e-mail", "junk email"]) {
            Self::Junk
        } else if special("\\Archive")
            || special("\\All")
            || named(&["archive", "archives", "all mail"])
        {
            Self::Archive
        } else {
            Self::Other
        }
    }

    /// The system labels a message in a folder of this role carries.
    pub fn labels(self) -> &'static [&'static str] {
        match self {
            // Important is the inbox for Exo, which reads `CATEGORY_PERSONAL` as "important".
            Self::Inbox => &["INBOX", "CATEGORY_PERSONAL"],
            Self::Sent => &["SENT"],
            Self::Trash => &["TRASH"],
            Self::Junk => &["SPAM"],
            Self::Drafts | Self::Archive | Self::Other => &[],
        }
    }
}

/// The label name of a folder with no role: its path, `/`-separated like Gmail's nested labels.
pub fn label_name(folder: &FolderInfo) -> String {
    match &folder.delimiter {
        Some(delimiter) => folder.name.replace(delimiter.as_str(), "/"),
        None => folder.name.clone(),
    }
}

/// The labels a message has: its folder's, plus those its flags give. `hidden` are the IDs of
/// [`HIDDEN`].
pub fn message_labels(
    folder_labels: &[String],
    flags: &[String],
    hidden: &[String],
) -> BTreeSet<String> {
    let has = |flag: &str| flags.iter().any(|f| f.eq_ignore_ascii_case(flag));
    let mut labels: BTreeSet<String> = folder_labels.iter().cloned().collect();
    if !has("\\Seen") {
        labels.insert("UNREAD".into());
    }
    if has("\\Flagged") {
        labels.insert("STARRED".into());
    }
    for (keyword, id) in KEYWORDS.iter().zip(hidden.iter().skip(1)) {
        if has(keyword) {
            labels.insert(id.clone());
        }
    }
    labels
}

/// The flags Exo's labels stand for, each with whether the message should carry it: the inverse
/// of [`message_labels`]'s flag half. Flags it does not list (`\Answered`...) are left alone.
pub fn wanted_flags(labels: &BTreeSet<String>, hidden: &[String]) -> Vec<(&'static str, bool)> {
    let mut flags = vec![
        ("\\Seen", !labels.contains("UNREAD")),
        ("\\Flagged", labels.contains("STARRED")),
    ];
    flags.extend(
        KEYWORDS
            .iter()
            .zip(hidden.iter().skip(1))
            .map(|(keyword, id)| (*keyword, labels.contains(id))),
    );
    flags
}
