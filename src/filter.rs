use nostr_sdk::Event;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SubscriptionFilter {
    pub ids: Option<Vec<String>>,
    pub authors: Option<Vec<String>>,
    pub kinds: Option<Vec<u16>>,
    pub search: Option<String>,
    #[serde(flatten)]
    #[serde(default)]
    pub tags: BTreeMap<String, Vec<String>>,
}

impl SubscriptionFilter {
    pub fn matches_event(&self, event: &Event) -> bool {
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
