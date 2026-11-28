//! Topics, topic filters, and the trie that matches an event's topic against
//! every subscription's filter.
//!
//! A topic is a slash-separated path of levels (`source/inbox/received`,
//! `agent/3f2a/done`). A filter is a topic that may use MQTT's wildcards as
//! whole levels: `+` matches exactly one level, and `#`, only as the last
//! level, matches the rest of the topic, including nothing (`agent/#`
//! matches `agent` and `agent/3f2a/done`).

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// The last level of a live-only topic `agent/<id>/<kind>`: the stream of a
/// turn's deltas, its tool progress, and the session's side runs (`/btw`,
/// `/better`), which never enter its history. Their events are fanned out to the
/// bus's listeners as they happen and never stored: they have no sequence
/// number (`Event::seq` is 0), a subscription to them replays nothing, and
/// a listener that falls behind loses the ones it missed. What a turn ends
/// with is published on a stored topic (`agent/<id>/done`).
const LIVE_ONLY_KINDS: &[&str] = &["stream", "tool", "side_question"];

/// The live-only feeds `runtime/<feed>`: the agents view's roster row
/// changes and agent traffic, and the model catalog's changes. A client reads
/// their current state with a query and follows the feed from then on, as it
/// follows a stream.
const LIVE_ONLY_FEEDS: &[&str] = &["roster", "traffic", "models"];

/// The topic the runtime announces its own end on, to every connection
/// before it closes them (the old daemon's `daemon_closing` push): live only,
/// so a client that connects to the next daemon never replays the last one's
/// end. Its payload is `{"reason": "shutdown" | "update", ...}`; the wire
/// server ends each connection with `closing` once it has delivered it.
pub const SYSTEM_CLOSING: &str = "system/closing";

/// The topic the runtime announces a distribution build's boot history
/// import on (the old daemon's `history_import` push): live only, so a
/// client never replays an import another boot ran. Its payload is the old
/// frame without `type` (`{"phase": "started" | "finished", "sessions",
/// ...}`); a client that connects mid-import reads the started one with the
/// `system.history_import` query.
pub const SYSTEM_HISTORY_IMPORT: &str = "system/history_import";

/// Most levels in a topic or filter.
pub const MAX_TOPIC_LEVELS: usize = 16;
/// Longest topic or filter, in bytes.
pub const MAX_TOPIC_BYTES: usize = 256;

/// Why a topic or filter is malformed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TopicError {
    #[error(
        "a topic has 1 to {MAX_TOPIC_LEVELS} levels and at most {MAX_TOPIC_BYTES} bytes: {0:?}"
    )]
    Size(String),
    #[error("topic {topic:?} has an empty level")]
    EmptyLevel { topic: String },
    #[error(
        "topic level {level:?} of {topic:?} is not letters, digits, `-`, `_`, `.`, `:`, `@` and `~`"
    )]
    Level { topic: String, level: String },
    #[error("`+` and `#` are filter wildcards, not topic levels: {0:?}")]
    WildcardInTopic(String),
    #[error("`#` is only a filter's last level: {0:?}")]
    HashNotLast(String),
}

/// The levels of `text`, checked for size, emptiness, and characters
/// (wildcard levels are passed through for the caller to judge).
fn levels(text: &str) -> Result<Vec<&str>, TopicError> {
    let levels: Vec<&str> = text.split('/').collect();
    if text.len() > MAX_TOPIC_BYTES || text.is_empty() || levels.len() > MAX_TOPIC_LEVELS {
        return Err(TopicError::Size(text.to_string()));
    }
    for level in &levels {
        if level.is_empty() {
            return Err(TopicError::EmptyLevel {
                topic: text.to_string(),
            });
        }
        let wildcard = *level == "+" || *level == "#";
        let plain = level
            .chars()
            .all(|letter| letter.is_ascii_alphanumeric() || "-_.:@~".contains(letter));
        if !wildcard && !plain {
            return Err(TopicError::Level {
                topic: text.to_string(),
                level: (*level).to_string(),
            });
        }
    }
    Ok(levels)
}

/// A concrete topic an event is published on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Topic(String);

impl Topic {
    /// The topic `text`.
    ///
    /// # Errors
    ///
    /// `text` is not 1 to 16 non-empty levels of allowed characters, or it
    /// uses a wildcard level.
    pub fn new(text: impl Into<String>) -> Result<Self, TopicError> {
        let text = text.into();
        if levels(&text)?
            .iter()
            .any(|level| *level == "+" || *level == "#")
        {
            return Err(TopicError::WildcardInTopic(text));
        }
        Ok(Topic(text))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The topic's levels, in order.
    pub fn levels(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// Whether events on this topic are live only, never stored: exactly
    /// `agent/<id>/stream`, `agent/<id>/tool`, `runtime/roster`,
    /// `runtime/traffic`, `runtime/models`, `runtime/session_list/<request>`
    /// (one saved-session list request's rows as it walks the store),
    /// [`SYSTEM_CLOSING`] or [`SYSTEM_HISTORY_IMPORT`].
    #[must_use]
    pub fn is_live_only(&self) -> bool {
        let mut levels = self.levels();
        match (levels.next(), levels.next(), levels.next(), levels.next()) {
            (Some("agent"), Some(_), Some(kind), None) => LIVE_ONLY_KINDS.contains(&kind),
            (Some("runtime"), Some(feed), None, None) => LIVE_ONLY_FEEDS.contains(&feed),
            (Some("runtime"), Some("session_list"), Some(_), None)
            | (Some("system"), Some("closing" | "history_import"), None, None) => true,
            _ => false,
        }
    }
}

impl fmt::Display for Topic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for Topic {
    type Error = TopicError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Topic::new(text)
    }
}

impl From<Topic> for String {
    fn from(topic: Topic) -> Self {
        topic.0
    }
}

/// One level of a filter.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FilterLevel {
    Exact(String),
    /// `+`: any one level.
    One,
    /// `#`: the rest of the topic, possibly nothing.
    Rest,
}

/// A topic pattern a subscription listens on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TopicFilter {
    text: String,
    levels: Vec<FilterLevel>,
}

impl TopicFilter {
    /// The filter `text`.
    ///
    /// # Errors
    ///
    /// `text` is not 1 to 16 non-empty levels of allowed characters or
    /// whole-level wildcards, or `#` is not its last level.
    pub fn new(text: impl Into<String>) -> Result<Self, TopicError> {
        let text = text.into();
        let raw = levels(&text)?;
        let last = raw.len() - 1;
        let mut parsed = Vec::with_capacity(raw.len());
        for (index, level) in raw.iter().enumerate() {
            parsed.push(match *level {
                "+" => FilterLevel::One,
                "#" if index == last => FilterLevel::Rest,
                "#" => return Err(TopicError::HashNotLast(text)),
                exact => FilterLevel::Exact(exact.to_string()),
            });
        }
        Ok(TopicFilter {
            text,
            levels: parsed,
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Whether `topic` is one this filter listens on.
    #[must_use]
    pub fn matches(&self, topic: &Topic) -> bool {
        let mut topic_levels = topic.levels();
        for level in &self.levels {
            match level {
                FilterLevel::Rest => return true,
                FilterLevel::One => {
                    if topic_levels.next().is_none() {
                        return false;
                    }
                }
                FilterLevel::Exact(exact) => {
                    if topic_levels.next() != Some(exact.as_str()) {
                        return false;
                    }
                }
            }
        }
        topic_levels.next().is_none()
    }
}

impl fmt::Display for TopicFilter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)
    }
}

impl TryFrom<String> for TopicFilter {
    type Error = TopicError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        TopicFilter::new(text)
    }
}

impl From<TopicFilter> for String {
    fn from(filter: TopicFilter) -> Self {
        filter.text
    }
}

/// Values (subscription ids, typically) keyed by filter, matched against a
/// topic in time proportional to the topic's depth rather than to the number
/// of filters.
#[derive(Debug, Clone)]
pub struct TopicTrie<V> {
    root: Node<V>,
    len: usize,
}

#[derive(Debug, Clone)]
struct Node<V> {
    exact: BTreeMap<String, Node<V>>,
    one: Option<Box<Node<V>>>,
    /// Values whose filter ends here with `#`.
    rest: Vec<V>,
    /// Values whose filter ends exactly here.
    here: Vec<V>,
}

impl<V> Default for Node<V> {
    fn default() -> Self {
        Node {
            exact: BTreeMap::new(),
            one: None,
            rest: Vec::new(),
            here: Vec::new(),
        }
    }
}

impl<V> Default for TopicTrie<V> {
    fn default() -> Self {
        TopicTrie {
            root: Node::default(),
            len: 0,
        }
    }
}

impl<V: PartialEq> TopicTrie<V> {
    #[must_use]
    pub fn new() -> Self {
        TopicTrie::default()
    }

    /// How many values the trie holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Add `value` under `filter`.
    pub fn insert(&mut self, filter: &TopicFilter, value: V) {
        let mut node = &mut self.root;
        for level in &filter.levels {
            match level {
                FilterLevel::Exact(exact) => node = node.exact.entry(exact.clone()).or_default(),
                FilterLevel::One => node = node.one.get_or_insert_with(Box::default),
                FilterLevel::Rest => {
                    node.rest.push(value);
                    self.len += 1;
                    return;
                }
            }
        }
        node.here.push(value);
        self.len += 1;
    }

    /// Remove one `value` stored under `filter`; whether there was one.
    pub fn remove(&mut self, filter: &TopicFilter, value: &V) -> bool {
        let mut node = &mut self.root;
        for level in &filter.levels {
            let next = match level {
                FilterLevel::Exact(exact) => node.exact.get_mut(exact),
                FilterLevel::One => node.one.as_deref_mut(),
                FilterLevel::Rest => {
                    return remove_one(&mut node.rest, value, &mut self.len);
                }
            };
            match next {
                Some(next) => node = next,
                None => return false,
            }
        }
        remove_one(&mut node.here, value, &mut self.len)
    }

    /// Every value whose filter matches `topic`, in no particular order.
    #[must_use]
    pub fn matches(&self, topic: &Topic) -> Vec<&V> {
        let levels: Vec<&str> = topic.levels().collect();
        let mut found = Vec::new();
        let mut frontier = vec![(&self.root, 0_usize)];
        while let Some((node, depth)) = frontier.pop() {
            found.extend(node.rest.iter());
            let Some(level) = levels.get(depth) else {
                found.extend(node.here.iter());
                continue;
            };
            if let Some(next) = node.exact.get(*level) {
                frontier.push((next, depth + 1));
            }
            if let Some(next) = node.one.as_deref() {
                frontier.push((next, depth + 1));
            }
        }
        found
    }
}

fn remove_one<V: PartialEq>(values: &mut Vec<V>, value: &V, len: &mut usize) -> bool {
    let Some(position) = values.iter().position(|held| held == value) else {
        return false;
    };
    values.swap_remove(position);
    *len -= 1;
    true
}

#[cfg(test)]
mod tests;
