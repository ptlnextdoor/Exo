//! The mailbox's change log, behind `history.list` and every `historyId`.

use gmail::{
    History, HistoryLabelAdded, HistoryLabelRemoved, HistoryMessageAdded, HistoryMessageDeleted,
    HistoryType, Message,
};

/// Records after the floor. Anything older has "expired", and `history.list` answers 404 for it.
#[derive(Debug, Clone)]
pub struct HistoryLog {
    records: Vec<Record>,
    /// Oldest `startHistoryId` still served.
    floor: u64,
    current: u64,
}

/// One change to one message.
#[derive(Debug, Clone)]
pub struct Record {
    pub id: u64,
    pub message_id: String,
    pub thread_id: String,
    /// The message's labels right after the change.
    pub labels: Vec<String>,
    pub change: Change,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    MessageAdded,
    MessageDeleted,
    LabelsAdded(Vec<String>),
    LabelsRemoved(Vec<String>),
}

impl HistoryLog {
    /// An empty log whose current ID is `start`.
    pub fn new(start: u64) -> Self {
        Self {
            records: Vec::new(),
            floor: start,
            current: start,
        }
    }

    /// A log that carries on from one a backing store kept: `records` ordered by ID, none older
    /// than `floor`.
    pub fn from_parts(floor: u64, current: u64, records: Vec<Record>) -> Self {
        Self {
            records,
            floor,
            current,
        }
    }

    /// Oldest `startHistoryId` still served.
    pub fn floor(&self) -> u64 {
        self.floor
    }

    /// The mailbox's current history ID.
    pub fn current(&self) -> u64 {
        self.current
    }

    /// Appends a record and returns its ID.
    pub fn push(
        &mut self,
        message_id: &str,
        thread_id: &str,
        labels: Vec<String>,
        change: Change,
    ) -> u64 {
        // Real IDs grow with gaps ("not contiguous with random gaps", per the discovery doc).
        // Skipping one now and then keeps clients from assuming otherwise.
        self.current += if self.current % 7 == 0 { 2 } else { 1 };
        self.records.push(Record {
            id: self.current,
            message_id: message_id.to_owned(),
            thread_id: thread_id.to_owned(),
            labels,
            change,
        });
        self.current
    }

    /// Records with an ID above `start`, or `None` if `start` predates retained history.
    pub fn since(&self, start: u64) -> Option<impl Iterator<Item = &Record>> {
        if start < self.floor {
            return None;
        }
        let first = self.records.partition_point(|record| record.id <= start);
        Some(self.records[first..].iter())
    }

    /// Drops all records, so every earlier `startHistoryId` is now expired.
    pub fn expire(&mut self) {
        self.records.clear();
        self.floor = self.current;
    }
}

impl Record {
    /// The `historyTypes` value this record belongs to.
    pub fn kind(&self) -> HistoryType {
        match self.change {
            Change::MessageAdded => HistoryType::MessageAdded,
            Change::MessageDeleted => HistoryType::MessageDeleted,
            Change::LabelsAdded(_) => HistoryType::LabelAdded,
            Change::LabelsRemoved(_) => HistoryType::LabelRemoved,
        }
    }

    /// Whether `labelId` filtering keeps this record.
    pub fn involves_label(&self, label: &str) -> bool {
        let changed = match &self.change {
            Change::LabelsAdded(ids) | Change::LabelsRemoved(ids) => ids.as_slice(),
            Change::MessageAdded | Change::MessageDeleted => &[],
        };
        self.labels.iter().chain(changed).any(|id| id == label)
    }

    /// The wire form.
    pub fn to_wire(&self) -> History {
        let reference = Message {
            id: Some(self.message_id.clone()),
            thread_id: Some(self.thread_id.clone()),
            ..Message::default()
        };
        let with_labels = Message {
            label_ids: self.labels.clone(),
            ..reference.clone()
        };
        let mut history = History {
            id: Some(self.id),
            messages: vec![reference.clone()],
            ..History::default()
        };
        match &self.change {
            Change::MessageAdded => {
                history.messages_added = vec![HistoryMessageAdded {
                    message: with_labels,
                }];
            }
            Change::MessageDeleted => {
                history.messages_deleted = vec![HistoryMessageDeleted { message: reference }];
            }
            Change::LabelsAdded(ids) => {
                history.labels_added = vec![HistoryLabelAdded {
                    message: with_labels,
                    label_ids: ids.clone(),
                }];
            }
            Change::LabelsRemoved(ids) => {
                history.labels_removed = vec![HistoryLabelRemoved {
                    message: with_labels,
                    label_ids: ids.clone(),
                }];
            }
        }
        history
    }
}
