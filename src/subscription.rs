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
            subscriber: fbs.subscriber().unwrap_or_default().to_string(),
        })
    }

    pub fn matches_event(&self, event: &Event) -> bool {
        if event.pubkey.to_hex() == self.subscriber {
            return false;
        }
        self.filter.matches_event(event)
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
        Ok(Self::deserialize(bytes)?.filter.matches_event(event))
    }
}
