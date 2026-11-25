//! Text search: Exo's `q=` on a message list is run by the engine, which only has the text of
//! messages that were opened. The server's own `SEARCH` finds the rest, and those messages are
//! then loaded for the engine to match.

use std::collections::HashMap;

use anyhow::Result;

use super::Facade;
use crate::mailstore::Mailstore;

/// Most messages per word and folder that a search loads.
const HITS: usize = 50;

/// The words of `query` that are not operators (`from:`, `-`, `OR`, ...), phrases whole.
fn free_text(query: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    for ch in query.chars() {
        match ch {
            '"' => quoted = !quoted,
            ch if ch.is_whitespace() && !quoted => words.push(std::mem::take(&mut word)),
            ch => word.push(ch),
        }
    }
    words.push(word);
    words
        .into_iter()
        .map(|word| word.trim_matches(['(', ')']).replace('\\', ""))
        .filter(|word| {
            !word.is_empty()
                && !word.contains(':')
                && !word.starts_with('-')
                && !matches!(word.as_str(), "OR" | "AND" | "AROUND")
        })
        .take(3)
        .collect()
}

impl<S: Mailstore> Facade<S> {
    /// The IDs of messages that the server finds the free text of `query` in.
    pub async fn text_hits(&self, query: &str) -> Result<Vec<String>> {
        let terms = free_text(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let mut state = self.state.lock().await;
        let mut ids = Vec::new();
        for folder in state.index.folders()? {
            let validity = state.store.select(&folder.name).await?.uid_validity;
            let known: HashMap<u32, String> = state
                .index
                .messages_in(&folder.name)?
                .into_iter()
                .filter(|row| row.uid_validity == validity)
                .map(|row| (row.uid, row.id))
                .collect();
            for term in &terms {
                let uids = state.store.search(&format!("TEXT \"{term}\"")).await?;
                ids.extend(
                    uids.iter()
                        .rev()
                        .take(HITS)
                        .filter_map(|uid| known.get(uid).cloned()),
                );
            }
        }
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::free_text;

    #[test]
    fn keeps_words_and_phrases_and_drops_operators() {
        assert_eq!(
            free_text(r#"from:bo (zebra OR "red fox") -spam label:x"#),
            ["zebra", "red fox"]
        );
        assert!(free_text("is:unread").is_empty());
    }
}
