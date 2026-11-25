//! The Gmail mailbox: messages, threads, labels, drafts, settings and the history log. Every
//! mutation goes through here so it bumps the history ID and leaves a history record.

pub mod history;
pub mod labels;
pub mod mime;
pub mod routes;
pub mod search;
mod wire;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use gmail::{Filter, Label, LabelType, SendAs, VacationSettings};
use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::error::{Failure, Result};
use history::{Change, HistoryLog};
use mime::Parsed;
use search::{Context, Query};

/// One user's mailbox.
#[derive(Debug, Clone)]
pub struct Mailbox {
    /// The account's address.
    pub email: String,
    /// The account's display name, used for `From:` when a sent message has none.
    pub name: String,
    /// The account's time zone, which date searches use.
    pub tz: TimeZone,
    messages: HashMap<String, StoredMessage>,
    /// Message IDs per thread, oldest first.
    threads: HashMap<String, Vec<String>>,
    /// System labels first, then user labels in creation order. Counts are computed on read.
    labels: Vec<Label>,
    user_labels_created: u32,
    /// Draft ID to message ID.
    drafts: BTreeMap<String, String>,
    pub history: HistoryLog,
    /// Source of unique IDs.
    sequence: u64,
    pub settings: Settings,
}

/// The settings resources.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub send_as: Vec<SendAs>,
    pub vacation: VacationSettings,
    pub filters: Vec<Filter>,
}

/// A message as stored: its bytes, labels, and everything Gmail derives from the bytes.
#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub id: String,
    pub thread_id: String,
    pub labels: BTreeSet<String>,
    /// Epoch milliseconds.
    pub internal_date: i64,
    pub history_id: u64,
    pub raw: Vec<u8>,
    pub mime: Parsed,
}

impl StoredMessage {
    /// Sort key for "newest first" listings and page tokens.
    pub fn sort_key(&self) -> (i64, u64) {
        (self.internal_date, numeric_id(&self.id))
    }
}

/// A message a backing store kept: [`Mailbox::restore`] puts it back as it was.
#[derive(Debug, Clone)]
pub struct Restored {
    pub id: String,
    pub thread_id: String,
    pub labels: BTreeSet<String>,
    /// Epoch milliseconds.
    pub internal_date: i64,
    pub history_id: u64,
    pub raw: Vec<u8>,
}

/// A message to add.
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub raw: Vec<u8>,
    pub labels: Vec<String>,
    pub threading: Threading,
    /// Epoch milliseconds.
    pub internal_date: i64,
    /// Run the user's filters, as SMTP delivery and `messages.import` do.
    pub apply_filters: bool,
}

/// How a new message finds its thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Threading {
    /// API calls: join the given thread when the threading headers and subject agree,
    /// otherwise start a new one (Gmail's documented rule).
    Requested(Option<String>),
    /// SMTP delivery: join the thread of a message named in `In-Reply-To`/`References`.
    Headers,
}

/// What `threads.list`, `messages.list` and `drafts.list` select.
pub struct Selection {
    pub query: Option<Query>,
    pub label_ids: Vec<String>,
    pub include_spam_trash: bool,
}

impl Mailbox {
    /// An empty mailbox whose history starts at `history_start`.
    pub fn new(email: &str, name: &str, tz: TimeZone, history_start: u64) -> Self {
        Self {
            email: email.to_owned(),
            name: name.to_owned(),
            tz,
            messages: HashMap::new(),
            threads: HashMap::new(),
            labels: labels::system(),
            user_labels_created: 0,
            drafts: BTreeMap::new(),
            history: HistoryLog::new(history_start),
            sequence: 0,
            settings: Settings::default(),
        }
    }

    /// Rejects `userId`s other than `me` and the account's address.
    pub fn check_user(&self, user: &str) -> Result<()> {
        if user == "me" || user.eq_ignore_ascii_case(&self.email) {
            Ok(())
        } else {
            Err(Failure::delegation_denied(user))
        }
    }

    /// Search context at time `now`.
    pub fn context(&self, now: Timestamp) -> Context<'_> {
        Context {
            labels: &self.labels,
            account: &self.email,
            now,
            tz: &self.tz,
        }
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    pub fn thread_count(&self) -> usize {
        self.threads.len()
    }

    // Lookups

    /// A message by ID: 400 for malformed IDs, 404 for unknown ones (as Gmail does).
    pub fn message(&self, id: &str) -> Result<&StoredMessage> {
        check_id(id)?;
        self.messages.get(id).ok_or_else(Failure::not_found)
    }

    /// A thread's messages, oldest first.
    pub fn thread(&self, id: &str) -> Result<Vec<&StoredMessage>> {
        check_id(id)?;
        let ids = self.threads.get(id).ok_or_else(Failure::not_found)?;
        Ok(ids.iter().map(|id| &self.messages[id]).collect())
    }

    /// Messages matching `selection`, newest first.
    pub fn select_messages(&self, selection: &Selection) -> Vec<&StoredMessage> {
        let skip_spam_trash = !selection.include_spam_trash
            && !selection
                .label_ids
                .iter()
                .any(|id| id == "SPAM" || id == "TRASH")
            && !selection
                .query
                .as_ref()
                .is_some_and(|query| query.mentions_spam_trash);
        let mut hits: Vec<&StoredMessage> = self
            .messages
            .values()
            .filter(|message| {
                !(skip_spam_trash
                    && (message.labels.contains("SPAM") || message.labels.contains("TRASH")))
            })
            .filter(|message| {
                selection
                    .label_ids
                    .iter()
                    .all(|id| message.labels.contains(id))
            })
            .filter(|message| {
                selection
                    .query
                    .as_ref()
                    .is_none_or(|query| query.matches(message))
            })
            .collect();
        hits.sort_by_key(|message| std::cmp::Reverse(message.sort_key()));
        hits
    }

    /// Threads with a matching message, ordered by their latest matching message.
    pub fn select_threads(&self, selection: &Selection) -> Vec<(&str, (i64, u64))> {
        let mut seen = BTreeSet::new();
        let mut threads: Vec<_> = self
            .select_messages(selection)
            .into_iter()
            .filter(|message| seen.insert(message.thread_id.as_str()))
            .map(|message| {
                (
                    message.thread_id.as_str(),
                    (message.internal_date, numeric_id(&message.thread_id)),
                )
            })
            .collect();
        // Page tokens need a strict order, including between threads whose latest messages tie.
        threads.sort_by_key(|(_, key)| std::cmp::Reverse(*key));
        threads
    }

    // Labels

    pub fn labels(&self) -> &[Label] {
        &self.labels
    }

    /// A label with its message and thread counts.
    pub fn label(&self, id: &str) -> Result<Label> {
        let label = self
            .labels
            .iter()
            .find(|label| label.id.as_deref() == Some(id))
            .ok_or_else(Failure::not_found)?;
        let mut threads_total = BTreeSet::new();
        let mut threads_unread = BTreeSet::new();
        let (mut messages_total, mut messages_unread) = (0, 0);
        for message in self.messages.values().filter(|m| m.labels.contains(id)) {
            messages_total += 1;
            threads_total.insert(&message.thread_id);
            if message.labels.contains("UNREAD") {
                messages_unread += 1;
                threads_unread.insert(&message.thread_id);
            }
        }
        let count = |n: usize| Some(u32::try_from(n).unwrap_or(u32::MAX));
        Ok(Label {
            messages_total: count(messages_total),
            messages_unread: count(messages_unread),
            threads_total: count(threads_total.len()),
            threads_unread: count(threads_unread.len()),
            ..label.clone()
        })
    }

    pub fn create_label(&mut self, request: Label) -> Result<Label> {
        let name = request.name.clone().unwrap_or_default();
        labels::validate_name(&name)?;
        self.check_name_free(&name, None)?;
        if let Some(color) = &request.color {
            labels::validate_color(color)?;
        }
        self.user_labels_created += 1;
        let label = Label {
            id: Some(format!("Label_{}", self.user_labels_created)),
            name: Some(name.trim().to_owned()),
            message_list_visibility: request
                .message_list_visibility
                .or(Some(gmail::MessageListVisibility::Show)),
            label_list_visibility: request
                .label_list_visibility
                .or(Some(gmail::LabelListVisibility::LabelShow)),
            kind: Some(LabelType::User),
            color: request.color,
            ..Label::default()
        };
        self.labels.push(label.clone());
        Ok(label)
    }

    /// `labels.update` (`patch == false`, replaces) and `labels.patch` (merges).
    pub fn update_label(&mut self, id: &str, request: Label, patch: bool) -> Result<Label> {
        let index = self.user_label_index(id)?;
        let current = self.labels[index].clone();
        let name = match (&request.name, patch) {
            (Some(name), _) => name.trim().to_owned(),
            (None, true) => current.name.clone().unwrap_or_default(),
            (None, false) => String::new(),
        };
        labels::validate_name(&name)?;
        self.check_name_free(&name, Some(id))?;
        let color = if patch {
            request.color.or(current.color)
        } else {
            request.color
        };
        if let Some(color) = &color {
            labels::validate_color(color)?;
        }
        let label = Label {
            name: Some(name),
            message_list_visibility: merge(
                patch,
                request.message_list_visibility,
                current.message_list_visibility,
            ),
            label_list_visibility: merge(
                patch,
                request.label_list_visibility,
                current.label_list_visibility,
            ),
            color,
            ..current
        };
        self.labels[index] = label.clone();
        Ok(label)
    }

    /// Deletes a user label and removes it from every message.
    pub fn delete_label(&mut self, id: &str) -> Result<()> {
        let index = self.user_label_index(id)?;
        let carrying: Vec<String> = self
            .messages
            .values()
            .filter(|message| message.labels.contains(id))
            .map(|message| message.id.clone())
            .collect();
        for message in carrying {
            self.apply_labels(&message, &[], &[id.to_owned()]);
        }
        self.labels.remove(index);
        Ok(())
    }

    fn user_label_index(&self, id: &str) -> Result<usize> {
        let index = self
            .labels
            .iter()
            .position(|label| label.id.as_deref() == Some(id))
            .ok_or_else(Failure::not_found)?;
        if self.labels[index].kind == Some(LabelType::System) {
            return Err(Failure::invalid_argument(format!(
                "Invalid update for system label: {id}"
            )));
        }
        Ok(index)
    }

    fn check_name_free(&self, name: &str, except: Option<&str>) -> Result<()> {
        let taken = self.labels.iter().any(|label| {
            label.id.as_deref() != except
                && label
                    .name
                    .as_deref()
                    .is_some_and(|other| other.eq_ignore_ascii_case(name.trim()))
        });
        if taken {
            return Err(Failure::new(
                409,
                "Label name exists or conflicts",
                "duplicate",
            ));
        }
        Ok(())
    }

    fn require_label(&self, id: &str) -> Result<()> {
        if self
            .labels
            .iter()
            .any(|label| label.id.as_deref() == Some(id))
        {
            Ok(())
        } else {
            Err(Failure::invalid_argument(format!("Invalid label: {id}")))
        }
    }

    // Messages

    /// Adds a message, recording `messagesAdded`. Returns its ID.
    pub fn insert(&mut self, new: NewMessage) -> Result<String> {
        let mut label_ids = new.labels;
        for label in &label_ids {
            self.require_label(label)?;
        }
        let id = self.next_message_id(new.internal_date);
        let mime = mime::parse(&new.raw, &id)
            .ok_or_else(|| Failure::invalid_argument("Invalid raw message"))?;
        let thread_id = self
            .resolve_thread(&new.threading, &mime)?
            .unwrap_or_else(|| id.clone());
        let mut message = StoredMessage {
            id: id.clone(),
            thread_id: thread_id.clone(),
            labels: BTreeSet::new(),
            internal_date: new.internal_date,
            history_id: 0,
            raw: new.raw,
            mime,
        };
        if new.apply_filters {
            self.apply_filters(&message, &mut label_ids);
        }
        message.labels = label_ids.into_iter().collect();

        let labels = message.labels.iter().cloned().collect();
        self.place(message);
        self.record(&id, labels, Change::MessageAdded);
        Ok(id)
    }

    /// Files `message` under its thread, oldest first, and stores it.
    fn place(&mut self, message: StoredMessage) {
        let thread = self.threads.entry(message.thread_id.clone()).or_default();
        let position =
            thread.partition_point(|other| self.messages[other].sort_key() <= message.sort_key());
        thread.insert(position, message.id.clone());
        self.messages.insert(message.id.clone(), message);
    }

    /// Puts back a message a backing store kept, with the ID, thread and history position it had.
    /// Nothing is recorded in the history.
    pub fn restore(&mut self, kept: Restored) -> Result<()> {
        let mime = mime::parse(&kept.raw, &kept.id)
            .ok_or_else(|| Failure::invalid_argument("Invalid raw message"))?;
        // New IDs must not repeat a restored one's low bits.
        self.sequence = self.sequence.max(numeric_id(&kept.id) & 0xF_FFFF);
        self.place(StoredMessage {
            id: kept.id,
            thread_id: kept.thread_id,
            labels: kept.labels,
            internal_date: kept.internal_date,
            history_id: kept.history_id,
            raw: kept.raw,
            mime,
        });
        Ok(())
    }

    /// Swaps in the bytes of a message that was held without its body. Nothing is recorded.
    pub fn replace_raw(&mut self, id: &str, raw: Vec<u8>) -> Result<()> {
        let mime = mime::parse(&raw, id)
            .ok_or_else(|| Failure::invalid_argument("Invalid raw message"))?;
        let message = self.messages.get_mut(id).ok_or_else(Failure::not_found)?;
        message.raw = raw;
        message.mime = mime;
        Ok(())
    }

    fn resolve_thread(&self, threading: &Threading, mime: &Parsed) -> Result<Option<String>> {
        let same_subject = |message: &StoredMessage| {
            base_subject(&message.mime.subject) == base_subject(&mime.subject)
        };
        match threading {
            Threading::Requested(None) => Ok(None),
            Threading::Requested(Some(thread_id)) => {
                let thread = self.thread(thread_id)?;
                let joins = !mime.ancestors.is_empty() && thread.iter().any(|m| same_subject(m));
                Ok(joins.then(|| thread_id.clone()))
            }
            Threading::Headers => Ok(mime.ancestors.iter().rev().find_map(|ancestor| {
                self.messages
                    .values()
                    .find(|m| m.mime.rfc822_id.as_deref() == Some(ancestor) && same_subject(m))
                    .map(|m| m.thread_id.clone())
            })),
        }
    }

    /// Applies matching filters' label actions (forwarding is not simulated).
    fn apply_filters(&self, message: &StoredMessage, label_ids: &mut Vec<String>) {
        let context =
            self.context(Timestamp::from_millisecond(message.internal_date).unwrap_or_default());
        for filter in &self.settings.filters {
            let (Some(criteria), Some(action)) = (&filter.criteria, &filter.action) else {
                continue;
            };
            if !Query::compile(&filter_query(criteria), &context).matches(message) {
                continue;
            }
            label_ids.retain(|id| !action.remove_label_ids.contains(id));
            for id in &action.add_label_ids {
                if !label_ids.contains(id) {
                    label_ids.push(id.clone());
                }
            }
        }
    }

    /// `messages.modify`, after validating the labels.
    pub fn modify(&mut self, id: &str, add: &[String], remove: &[String]) -> Result<()> {
        self.message(id)?;
        self.check_modifiable(add, remove)?;
        self.apply_labels(id, add, remove);
        Ok(())
    }

    /// `threads.modify`.
    pub fn modify_thread(&mut self, id: &str, add: &[String], remove: &[String]) -> Result<()> {
        let ids = self.thread_message_ids(id)?;
        self.check_modifiable(add, remove)?;
        for message in ids {
            self.apply_labels(&message, add, remove);
        }
        Ok(())
    }

    fn check_modifiable(&self, add: &[String], remove: &[String]) -> Result<()> {
        for label in add.iter().chain(remove) {
            self.require_label(label)?;
            if labels::is_immutable(label) {
                return Err(Failure::invalid_argument(format!("Invalid label: {label}")));
            }
        }
        Ok(())
    }

    /// `messages.trash`. Trashing takes a message out of the inbox and out of spam.
    pub fn trash(&mut self, id: &str) -> Result<()> {
        self.message(id)?;
        self.apply_labels(id, &["TRASH".into()], &["INBOX".into(), "SPAM".into()]);
        Ok(())
    }

    /// `messages.untrash`. Gmail does not put the message back in the inbox.
    pub fn untrash(&mut self, id: &str) -> Result<()> {
        self.message(id)?;
        self.apply_labels(id, &[], &["TRASH".into()]);
        Ok(())
    }

    pub fn thread_message_ids(&self, id: &str) -> Result<Vec<String>> {
        Ok(self.thread(id)?.iter().map(|m| m.id.clone()).collect())
    }

    /// Changes labels without validation, recording one history record per kind of change.
    pub fn apply_labels(&mut self, id: &str, add: &[String], remove: &[String]) {
        let Some(message) = self.messages.get_mut(id) else {
            return;
        };
        let added: Vec<String> = add
            .iter()
            .filter(|label| message.labels.insert((*label).clone()))
            .cloned()
            .collect();
        let removed: Vec<String> = remove
            .iter()
            .filter(|label| !add.contains(label) && message.labels.remove(*label))
            .cloned()
            .collect();
        let labels: Vec<String> = message.labels.iter().cloned().collect();
        if !added.is_empty() {
            self.record(id, labels.clone(), Change::LabelsAdded(added));
        }
        if !removed.is_empty() {
            self.record(id, labels, Change::LabelsRemoved(removed));
        }
    }

    /// Permanently deletes a message (and its draft, if it is one).
    pub fn delete(&mut self, id: &str) -> Result<()> {
        self.message(id)?;
        let message = self.messages.remove(id).expect("checked above");
        if let Some(thread) = self.threads.get_mut(&message.thread_id) {
            thread.retain(|other| other != id);
            if thread.is_empty() {
                self.threads.remove(&message.thread_id);
            }
        }
        self.drafts.retain(|_, message_id| message_id != id);
        self.history
            .push(id, &message.thread_id, Vec::new(), Change::MessageDeleted);
        Ok(())
    }

    /// The bytes of an attachment.
    pub fn attachment(&self, message_id: &str, attachment_id: &str) -> Result<&[u8]> {
        self.message(message_id)?
            .mime
            .attachments
            .get(attachment_id)
            .map(Vec::as_slice)
            .ok_or_else(|| Failure::invalid_argument("Invalid attachment token"))
    }

    // Sending

    /// `messages.send`: fills in `From`, `Date` and `Message-ID` like Gmail, then files the
    /// message under SENT (and INBOX when addressed to the account itself).
    pub fn send(&mut self, raw: Vec<u8>, thread_id: Option<String>, now: i64) -> Result<String> {
        let probe = mime::parse(&raw, "probe")
            .ok_or_else(|| Failure::invalid_argument("Invalid raw message"))?;
        if probe.recipients.is_empty() {
            return Err(Failure::invalid_argument("Recipient address required"));
        }
        let mut headers = String::new();
        if probe.payload.header("From").is_none() {
            let from = gmail::mime::Mailbox::new(&self.name, &self.email);
            headers.push_str(&format!("From: {from}\r\n"));
        }
        if probe.payload.header("Date").is_none() {
            let date = Timestamp::from_millisecond(now)
                .unwrap_or_default()
                .to_zoned(self.tz.clone());
            if let Ok(date) = jiff::fmt::rfc2822::to_string(&date) {
                headers.push_str(&format!("Date: {date}\r\n"));
            }
        }
        if probe.rfc822_id.is_none() {
            self.sequence += 1;
            headers.push_str(&format!(
                "Message-ID: <CAExo{:x}{:x}@mail.gmail.com>\r\n",
                now, self.sequence
            ));
        }
        let mut stamped = headers.into_bytes();
        stamped.extend_from_slice(&raw);

        let mut labels = vec!["SENT".to_owned()];
        if probe
            .recipients
            .iter()
            .any(|to| to.eq_ignore_ascii_case(&self.email))
        {
            labels.push("INBOX".to_owned());
        }
        self.insert(NewMessage {
            raw: stamped,
            labels,
            threading: Threading::Requested(thread_id),
            internal_date: now,
            apply_filters: false,
        })
    }

    // Drafts

    /// `(draft ID, message)` pairs, newest message first.
    pub fn drafts(&self) -> Vec<(&str, &StoredMessage)> {
        let mut drafts: Vec<_> = self
            .drafts
            .iter()
            .filter_map(|(draft, message)| Some((draft.as_str(), self.messages.get(message)?)))
            .collect();
        drafts.sort_by_key(|(_, message)| std::cmp::Reverse(message.sort_key()));
        drafts
    }

    pub fn draft(&self, id: &str) -> Result<&StoredMessage> {
        let message = self.drafts.get(id).ok_or_else(Failure::not_found)?;
        self.message(message)
    }

    pub fn create_draft(
        &mut self,
        raw: Vec<u8>,
        thread_id: Option<String>,
        now: i64,
    ) -> Result<String> {
        let message = self.insert_draft_message(raw, thread_id, now)?;
        self.sequence += 1;
        let id = format!(
            "r{}",
            (self.sequence.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 1) % 10_000_000_000_000_000_000
        );
        self.drafts.insert(id.clone(), message);
        Ok(id)
    }

    /// Replaces the draft's message. The draft keeps its ID, and the message gets a new one.
    pub fn update_draft(
        &mut self,
        id: &str,
        raw: Vec<u8>,
        thread_id: Option<String>,
        now: i64,
    ) -> Result<()> {
        let old = self.draft(id)?.id.clone();
        let thread_id = thread_id.or_else(|| Some(self.messages[&old].thread_id.clone()));
        let message = self.insert_draft_message(raw, thread_id, now)?;
        self.delete(&old)?;
        self.drafts.insert(id.to_owned(), message);
        Ok(())
    }

    fn insert_draft_message(
        &mut self,
        raw: Vec<u8>,
        thread_id: Option<String>,
        now: i64,
    ) -> Result<String> {
        self.insert(NewMessage {
            raw,
            labels: vec!["DRAFT".into()],
            threading: Threading::Requested(thread_id),
            internal_date: now,
            apply_filters: false,
        })
    }

    pub fn delete_draft(&mut self, id: &str) -> Result<()> {
        let message = self.draft(id)?.id.clone();
        self.delete(&message)
    }

    /// Sends a draft as last saved and deletes it. Returns the sent message's ID.
    pub fn send_draft(&mut self, id: &str, now: i64) -> Result<String> {
        let draft = self.draft(id)?;
        let (raw, thread_id, draft_message) =
            (draft.raw.clone(), draft.thread_id.clone(), draft.id.clone());
        // The draft was already threaded on creation, so its thread can be requested as is.
        let thread = (thread_id != draft_message).then_some(thread_id);
        let sent = self.send(raw, thread, now)?;
        self.delete(&draft_message)?;
        Ok(sent)
    }

    // History

    fn record(&mut self, id: &str, labels: Vec<String>, change: Change) {
        let Some(thread_id) = self.messages.get(id).map(|m| m.thread_id.clone()) else {
            return;
        };
        let history_id = self.history.push(id, &thread_id, labels, change);
        if let Some(message) = self.messages.get_mut(id) {
            message.history_id = history_id;
        }
    }

    // IDs

    /// Gmail message IDs are hex, with the receive time in the high bits.
    fn next_message_id(&mut self, internal_date: i64) -> String {
        self.sequence += 1;
        let millis = u64::try_from(internal_date).unwrap_or_default();
        format!("{:x}", (millis << 20) | (self.sequence & 0xF_FFFF))
    }

    /// A fresh opaque ID for settings resources (filters).
    pub fn next_opaque_id(&mut self, prefix: &str) -> String {
        self.sequence += 1;
        let hash = self.sequence.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        format!("{prefix}{hash:016x}")
    }
}

/// `patch` keeps the old value where the request has none, and `update` takes the request as is.
fn merge<T>(patch: bool, new: Option<T>, old: Option<T>) -> Option<T> {
    if patch { new.or(old) } else { new }
}

/// The search query equivalent to a filter's criteria.
fn filter_query(criteria: &gmail::FilterCriteria) -> String {
    let mut terms = Vec::new();
    let mut push = |op: &str, value: &Option<String>| {
        if let Some(value) = value.as_deref().filter(|v| !v.is_empty()) {
            terms.push(format!("{op}({value})"));
        }
    };
    push("from:", &criteria.from);
    push("to:", &criteria.to);
    push("subject:", &criteria.subject);
    push("", &criteria.query);
    push("-", &criteria.negated_query);
    if criteria.has_attachment == Some(true) {
        terms.push("has:attachment".into());
    }
    if let Some(size) = criteria.size {
        match criteria.size_comparison {
            Some(gmail::SizeComparison::Smaller) => terms.push(format!("smaller:{size}")),
            Some(gmail::SizeComparison::Larger) => terms.push(format!("larger:{size}")),
            _ => {}
        }
    }
    terms.join(" ")
}

/// Gmail answers 400 "Invalid id value" for IDs that can't be message or thread IDs.
fn check_id(id: &str) -> Result<()> {
    if !id.is_empty() && id.len() <= 16 && id.chars().all(|ch| ch.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(Failure::invalid_argument("Invalid id value"))
    }
}

pub fn numeric_id(id: &str) -> u64 {
    u64::from_str_radix(id, 16).unwrap_or_default()
}

/// The subject with reply/forward prefixes removed, for thread matching.
fn base_subject(subject: &str) -> String {
    let mut rest = subject.trim();
    loop {
        let lower = rest.to_ascii_lowercase();
        let Some(prefix) = ["re:", "fwd:", "fw:", "aw:", "sv:"]
            .iter()
            .find(|prefix| lower.starts_with(**prefix))
        else {
            return rest.to_lowercase();
        };
        rest = rest[prefix.len()..].trim_start();
    }
}

/// Opaque page tokens: the sort key of the last item served, as digits like Gmail's.
pub mod page_token {
    use crate::error::{Failure, Result};

    pub fn encode((millis, id): (i64, u64)) -> String {
        format!("{millis:013}{id:020}")
    }

    pub fn decode(token: &str) -> Result<(i64, u64)> {
        let invalid = || Failure::invalid_argument("Invalid pageToken");
        if token.len() != 33 || !token.chars().all(|ch| ch.is_ascii_digit()) {
            return Err(invalid());
        }
        let millis = token[..13].parse().map_err(|_| invalid())?;
        let id = token[13..].parse().map_err(|_| invalid())?;
        Ok((millis, id))
    }

    /// The page after `token` from `items` (sorted newest first), and the next token.
    pub fn page<T>(
        items: Vec<T>,
        key: impl Fn(&T) -> (i64, u64),
        token: Option<&str>,
        size: usize,
    ) -> Result<(Vec<T>, Option<String>)> {
        let after = token.map(decode).transpose()?;
        let mut rest = items
            .into_iter()
            .filter(|item| after.is_none_or(|cursor| key(item) < cursor))
            .peekable();
        let page: Vec<T> = rest.by_ref().take(size).collect();
        let next = rest.peek().and(page.last()).map(|last| encode(key(last)));
        Ok((page, next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_subject_strips_reply_prefixes() {
        assert_eq!(base_subject("Re: Fwd: RE: Lunch?"), "lunch?");
    }

    #[test]
    fn page_tokens_round_trip() {
        let key = (1_794_924_000_000, 0x19a0_0000_0000_0001);
        assert_eq!(page_token::decode(&page_token::encode(key)).unwrap(), key);
        assert!(page_token::decode("nope").is_err());
    }
}
