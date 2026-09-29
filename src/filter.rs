use nostr_sdk::Event;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SubscriptionFilter {
    pub ids: Option<Vec<String>>,
    pub authors: Option<Vec<String>>,
    pub kinds: Option<Vec<u16>>,
    pub search: Option<String>,
    /// Inclusive event time bound; future bounds also delay delivery until wall time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    #[serde(flatten)]
    #[serde(default)]
    pub tags: BTreeMap<String, Vec<String>>,
}

impl SubscriptionFilter {
    pub fn matches_event(&self, event: &Event) -> bool {
        self.matches_event_at(event, nostr_sdk::Timestamp::now().as_secs())
    }

    fn matches_event_at(&self, event: &Event, now: u64) -> bool {
        let created_at = event.created_at.as_secs();
        if self
            .since
            .is_some_and(|since| now < since || created_at < since)
            || self.until.is_some_and(|until| created_at > until)
        {
            return false;
        }
        if let Some(ids) = &self.ids {
            if !ids.contains(&event.id.to_hex()) {
                return false;
            }
        }

        if let Some(authors) = &self.authors {
            if !authors.contains(&event.pubkey.to_hex()) {
                return false;
            }
        }

        if let Some(kinds) = &self.kinds {
            if !kinds.contains(&event.kind.as_u16()) {
                return false;
            }
        }

        if let Some(search) = &self.search {
            if !event
                .content
                .to_lowercase()
                .contains(&search.to_lowercase())
            {
                return false;
            }
        }

        for (tag_name, tag_values) in &self.tags {
            if let Some(tag_name) = tag_name.strip_prefix('#') {
                let event_tag_values: Vec<_> = event
                    .tags
                    .iter()
                    .filter(|tag| {
                        tag.as_slice()
                            .first()
                            .map(|t| t == tag_name)
                            .unwrap_or(false)
                    })
                    .filter_map(|tag| tag.as_slice().get(1).cloned())
                    .collect();

                if !tag_values
                    .iter()
                    .any(|value| event_tag_values.contains(value))
                {
                    return false;
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::{EventBuilder, Keys, Kind, Timestamp};

    #[test]
    fn timed_filter_survives_storage_and_resumes_at_deadline_without_an_app_refresh() {
        let keys = Keys::generate();
        let subscription: crate::subscription::Subscription =
            serde_json::from_value(serde_json::json!({
                "filter": {"authors": [keys.public_key().to_hex()], "kinds": [1060], "since": 200},
                "apns_tokens": ["test-token"], "subscriber": "subscriber"
            }))
            .unwrap();
        let restored =
            crate::subscription::Subscription::deserialize(&subscription.serialize().unwrap())
                .unwrap();
        let event = |at| {
            EventBuilder::new(Kind::Custom(1060), "encrypted")
                .custom_created_at(Timestamp::from(at))
                .sign_with_keys(&keys)
                .unwrap()
        };
        assert!(
            !restored.filter.matches_event_at(&event(200), 199),
            "future-dated event cannot bypass active mute"
        );
        assert!(
            !restored.filter.matches_event_at(&event(199), 200),
            "muted backlog stays silent"
        );
        assert!(restored.filter.matches_event_at(&event(200), 200));
        assert!(restored.filter.matches_event_at(&event(201), 201));
    }

    #[test]
    fn legacy_filters_and_inclusive_until_are_preserved() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(1060), "encrypted")
            .custom_created_at(Timestamp::from(200))
            .sign_with_keys(&keys)
            .unwrap();
        let legacy: SubscriptionFilter =
            serde_json::from_value(serde_json::json!({"kinds":[1060]})).unwrap();
        assert!(legacy.matches_event_at(&event, 200));
        let mut bounded = legacy;
        bounded.until = Some(200);
        assert!(bounded.matches_event_at(&event, 200));
        bounded.until = Some(199);
        assert!(!bounded.matches_event_at(&event, 200));
    }
}
