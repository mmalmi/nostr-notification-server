use std::error::Error;

use nostr_sdk::Event;
use serde::{Deserialize, Serialize};

use crate::filter::SubscriptionFilter;
use crate::schema::subscription_generated::subscription::root_as_subscription;
use crate::web_push::WebPushSubscription;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Subscription {
    #[serde(default)]
    pub webhooks: Vec<String>,
    #[serde(default)]
    pub web_push_subscriptions: Vec<WebPushSubscription>,
    #[serde(default)]
    pub fcm_tokens: Vec<String>,
    #[serde(default)]
    pub apns_tokens: Vec<String>,
    #[serde(default)]
    pub social_graph_filter: bool,
    pub filter: SubscriptionFilter,
    #[serde(default)]
    pub filters: Vec<SubscriptionFilter>,
    #[serde(default)]
    pub subscriber: String,
}

impl Subscription {
    pub fn is_empty(&self) -> bool {
        self.webhooks.is_empty()
            && self.web_push_subscriptions.is_empty()
            && self.fcm_tokens.is_empty()
            && self.apns_tokens.is_empty()
    }

    pub fn serialize(&self) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self, Box<dyn Error + Send + Sync>> {
        if let Ok(subscription) = serde_json::from_slice::<Self>(bytes) {
            return Ok(subscription);
        }
        Self::deserialize_flatbuffer(bytes)
    }

    fn deserialize_flatbuffer(bytes: &[u8]) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let fbs = root_as_subscription(bytes)?;

        let webhooks = fbs
            .webhooks()
            .map(|items| items.iter().map(|value| value.to_string()).collect())
            .unwrap_or_default();

        let web_push_subscriptions = fbs
            .web_push_subscriptions()
            .map(|subs| {
                subs.iter()
                    .map(|sub| WebPushSubscription {
                        endpoint: sub.endpoint().unwrap_or_default().to_string(),
                        auth: sub.auth().unwrap_or_default().to_string(),
                        p256dh: sub.p256dh().unwrap_or_default().to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let filter = {
            let f = fbs.filter().expect("Filter should always be present");
            SubscriptionFilter {
                ids: f
                    .ids()
                    .map(|ids| ids.iter().map(|s| s.to_string()).collect()),
                authors: f
                    .authors()
                    .map(|authors| authors.iter().map(|s| s.to_string()).collect()),
                kinds: f.kinds().map(|kinds| kinds.iter().collect()),
                search: f.search().map(|s| s.to_string()),
                tags: f
                    .tags()
                    .map(|tags| {
                        tags.iter()
                            .filter_map(|tag| {
                                let key = tag.key()?.to_string();
                                let values = tag.values()?.iter().map(|s| s.to_string()).collect();
                                Some((key, values))
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        };

        Ok(Self {
            webhooks,
            web_push_subscriptions,
            fcm_tokens: Vec::new(),
            apns_tokens: Vec::new(),
            social_graph_filter: false,
            filter,
            filters: Vec::new(),
            subscriber: fbs.subscriber().unwrap_or_default().to_string(),
        })
    }

    pub fn effective_filters(&self) -> impl Iterator<Item = &SubscriptionFilter> {
        std::iter::once(&self.filter).chain(self.filters.iter())
    }

    fn matches_any_filter(&self, event: &Event) -> bool {
        self.effective_filters()
            .any(|filter| filter.matches_event(event))
    }

    pub fn matches_event(&self, event: &Event) -> bool {
        if event.pubkey.to_hex() == self.subscriber {
            return false;
        }
        self.matches_any_filter(event)
    }

    pub fn merge(&mut self, other: Subscription) {
        self.webhooks.extend(other.webhooks);

        for new_sub in other.web_push_subscriptions {
            if !self
                .web_push_subscriptions
                .iter()
                .any(|existing| existing.endpoint == new_sub.endpoint)
            {
                self.web_push_subscriptions.push(new_sub);
            }
        }

        for token in other.fcm_tokens {
            if !self.fcm_tokens.contains(&token) {
                self.fcm_tokens.push(token);
            }
        }

        for token in other.apns_tokens {
            if !self.apns_tokens.contains(&token) {
                self.apns_tokens.push(token);
            }
        }
    }

    pub fn matches_event_filter_only(
        bytes: &[u8],
        event: &Event,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        Ok(Self::deserialize(bytes)?.matches_any_filter(event))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use nostr_sdk::{EventBuilder, Keys, Kind, Tag};

    fn filter(
        authors: Option<Vec<String>>,
        kinds: Option<Vec<u16>>,
        tags: BTreeMap<String, Vec<String>>,
    ) -> SubscriptionFilter {
        SubscriptionFilter {
            ids: None,
            authors,
            kinds,
            search: None,
            tags,
        }
    }

    #[test]
    fn matches_event_accepts_any_effective_filter() {
        let message_keys = Keys::generate();
        let invite_recipient = Keys::generate().public_key().to_hex();
        let subscriber = Keys::generate().public_key().to_hex();

        let mut invite_tags = BTreeMap::new();
        invite_tags.insert("#p".to_string(), vec![invite_recipient.clone()]);

        let subscription = Subscription {
            webhooks: Vec::new(),
            web_push_subscriptions: Vec::new(),
            fcm_tokens: vec!["token".to_string()],
            apns_tokens: Vec::new(),
            social_graph_filter: false,
            filter: filter(
                Some(vec![message_keys.public_key().to_hex()]),
                Some(vec![1060]),
                BTreeMap::new(),
            ),
            filters: vec![filter(None, Some(vec![1059]), invite_tags)],
            subscriber,
        };

        let message_event = EventBuilder::new(Kind::from(1060), "ciphertext", [])
            .to_event(&message_keys)
            .expect("message event");
        let invite_tag = Tag::parse(&["p", invite_recipient.as_str()]).expect("p tag");
        let invite_event = EventBuilder::new(Kind::from(1059), "ciphertext", [invite_tag])
            .to_event(&Keys::generate())
            .expect("invite response event");
        let unrelated_event = EventBuilder::new(Kind::from(1060), "ciphertext", [])
            .to_event(&Keys::generate())
            .expect("unrelated event");

        assert!(subscription.matches_event(&message_event));
        assert!(subscription.matches_event(&invite_event));
        assert!(!subscription.matches_event(&unrelated_event));
    }
}
