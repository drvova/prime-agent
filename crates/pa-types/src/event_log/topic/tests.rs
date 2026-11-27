use proptest::prelude::*;

use super::{Topic, TopicError, TopicFilter, TopicTrie};

/// Only a session's stream and tool topics are live only.
#[test]
fn only_agent_stream_and_tool_topics_are_live_only() {
    assert_eq!(
        [
            "agent/3f2a/stream",
            "agent/3f2a/tool",
            "agent/3f2a/done",
            "agent/3f2a/stream/more",
            "source/3f2a/stream",
            "agent/stream",
        ]
        .map(|topic| Topic::new(topic).unwrap().is_live_only()),
        [true, true, false, false, false, false]
    );
}

/// Which topics and filters parse, and why the rest do not.
#[test]
fn topics_and_filters_are_validated() {
    let parsed = |text: &str| (Topic::new(text).err(), TopicFilter::new(text).err());
    assert_eq!(
        [
            "agent/3f2a/done",
            "source/inbox/+",
            "agent/#",
            "a//b",
            "a/#/b",
            "a/b c",
            "",
        ]
        .map(parsed),
        [
            (None, None),
            (
                Some(TopicError::WildcardInTopic("source/inbox/+".to_string())),
                None
            ),
            (
                Some(TopicError::WildcardInTopic("agent/#".to_string())),
                None
            ),
            (
                Some(TopicError::EmptyLevel {
                    topic: "a//b".to_string()
                }),
                Some(TopicError::EmptyLevel {
                    topic: "a//b".to_string()
                }),
            ),
            (
                Some(TopicError::WildcardInTopic("a/#/b".to_string())),
                Some(TopicError::HashNotLast("a/#/b".to_string())),
            ),
            (
                Some(TopicError::Level {
                    topic: "a/b c".to_string(),
                    level: "b c".to_string()
                }),
                Some(TopicError::Level {
                    topic: "a/b c".to_string(),
                    level: "b c".to_string()
                }),
            ),
            (
                Some(TopicError::Size(String::new())),
                Some(TopicError::Size(String::new()))
            ),
        ]
    );
}

/// MQTT's wildcard semantics: `+` is exactly one level, `#` the rest
/// including nothing.
#[test]
fn a_filter_matches_as_mqtt_does() {
    let cases = [
        ("agent/+/done", "agent/3f2a/done", true),
        ("agent/+/done", "agent/done", false),
        ("agent/+/done", "agent/3f2a/b/done", false),
        ("agent/#", "agent", true),
        ("agent/#", "agent/3f2a/done", true),
        ("agent/#", "agents/3f2a", false),
        ("#", "timer/nightly", true),
        ("+", "timer", true),
        ("+", "timer/nightly", false),
        ("source/inbox", "source/inbox/received", false),
    ];
    let matched: Vec<bool> = cases
        .iter()
        .map(|(filter, topic, _)| {
            TopicFilter::new(*filter)
                .unwrap()
                .matches(&Topic::new(*topic).unwrap())
        })
        .collect();
    assert_eq!(
        matched,
        cases
            .iter()
            .map(|(_, _, expected)| *expected)
            .collect::<Vec<_>>()
    );
}

fn level() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["a", "b", "c"]).prop_map(str::to_string)
}

fn topic() -> impl Strategy<Value = Topic> {
    prop::collection::vec(level(), 1..5).prop_map(|levels| Topic::new(levels.join("/")).unwrap())
}

fn filter() -> impl Strategy<Value = TopicFilter> {
    (
        prop::collection::vec(prop_oneof![level(), Just("+".to_string())], 0..4),
        any::<bool>(),
    )
        .prop_filter_map("a filter has a level", |(mut levels, hash)| {
            if hash {
                levels.push("#".to_string());
            }
            (!levels.is_empty()).then(|| TopicFilter::new(levels.join("/")).unwrap())
        })
}

proptest! {
    /// The trie finds exactly the filters that match a topic one by one,
    /// and forgets the ones removed from it.
    #[test]
    fn the_trie_matches_what_each_filter_matches(
        filters in prop::collection::vec(filter(), 0..24),
        removed in prop::collection::vec(any::<bool>(), 24),
        topics in prop::collection::vec(topic(), 1..8),
    ) {
        let mut trie = TopicTrie::new();
        for (index, filter) in filters.iter().enumerate() {
            trie.insert(filter, index);
        }
        let mut kept = Vec::new();
        for (index, filter) in filters.iter().enumerate() {
            if removed[index] {
                prop_assert!(trie.remove(filter, &index));
            } else {
                kept.push((index, filter));
            }
        }
        prop_assert_eq!(trie.len(), kept.len());
        for topic in &topics {
            let mut found: Vec<usize> = trie.matches(topic).into_iter().copied().collect();
            found.sort_unstable();
            let expected: Vec<usize> = kept
                .iter()
                .filter(|(_, filter)| filter.matches(topic))
                .map(|(index, _)| *index)
                .collect();
            prop_assert_eq!(found, expected);
        }
    }
}
