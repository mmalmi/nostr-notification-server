use serde::{Deserialize, Serialize};
use nostr_sdk::Event;
use crate::web_push::WebPushSubscription;
use std::error::Error;
use crate::schema::subscription_generated::subscription::{
    self,
    Subscription as FbsSubscription,
    SubscriptionArgs,
    SubscriptionFilter as FbsFilter,
    SubscriptionFilterArgs,
    Tag as FbsTag,
    TagArgs,
    root_as_subscription,
};
use flatbuffers::FlatBufferBuilder;
use crate::filter::SubscriptionFilter;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Subscription {
    pub webhooks: Vec<String>,
    pub web_push_subscriptions: Vec<WebPushSubscription>,
    pub filter: SubscriptionFilter,
}

impl Subscription {
    fn serialize_filter<'a>(&self, builder: &mut FlatBufferBuilder<'a>) -> Result<flatbuffers::WIPOffset<FbsFilter<'a>>, Box<dyn Error + Send + Sync>> {
        // Serialize ids
        let ids = self.filter.ids.as_ref().map(|ids| {
            let ids: Vec<_> = ids.iter()
                .map(|id| builder.create_string(id))
                .collect();
            builder.create_vector(&ids)
        });

        // Serialize authors
        let authors = self.filter.authors.as_ref().map(|authors| {
            let authors: Vec<_> = authors.iter()
                .map(|author| builder.create_string(author))
                .collect();
            builder.create_vector(&authors)
        });

        // Serialize kinds
        let kinds = self.filter.kinds.as_ref().map(|kinds| {
            builder.create_vector(kinds)
        });

        // Serialize search
        let search = self.filter.search.as_ref().map(|s| builder.create_string(s));

        // Serialize tags
        let tags = {
            let mut tag_vec = Vec::new();
            for (key, values) in &self.filter.tags {
                let key_offset = builder.create_string(key);
                let values: Vec<_> = values.iter()
                    .map(|v| builder.create_string(v))
                    .collect();
                let values_vec = builder.create_vector(&values);

                let tag = FbsTag::create(
                    builder,
                    &TagArgs {
                        key: Some(key_offset),
                        values: Some(values_vec),
                    }
                );
                tag_vec.push(tag);
            }
            let tags_vector = builder.create_vector(&tag_vec);
            Some(tags_vector)
        };

        Ok(FbsFilter::create(
            builder,
            &SubscriptionFilterArgs {
                ids,
                authors,
                kinds,
                search,
                tags,
            }
        ))
    }

    pub fn serialize(&self) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        let mut builder = FlatBufferBuilder::new();
        
        // Serialize webhooks
        let webhooks: Vec<_> = self.webhooks.iter()
            .map(|url| builder.create_string(url))
            .collect();
        let webhooks_vec = builder.create_vector(&webhooks);
        
        // Serialize web push subscriptions
        let web_push_subs: Vec<_> = self.web_push_subscriptions.iter()
            .map(|sub| {
                let endpoint = builder.create_string(&sub.endpoint);
                let auth = builder.create_string(&sub.auth);
                let p256dh = builder.create_string(&sub.p256dh);
                
                subscription::WebPushSubscription::create(
                    &mut builder,
                    &subscription::WebPushSubscriptionArgs {
                        endpoint: Some(endpoint),
                        auth: Some(auth),
                        p256dh: Some(p256dh),
                    }
                )
            })
            .collect();
        let web_push_vec = builder.create_vector(&web_push_subs);
        
        // Serialize filter first
        let filter = self.serialize_filter(&mut builder)?;
        
        // Create subscription
        let subscription = FbsSubscription::create(
            &mut builder,
            &SubscriptionArgs {
                webhooks: Some(webhooks_vec),
                web_push_subscriptions: Some(web_push_vec),
                filter: Some(filter),
            }
        );
        
        builder.finish(subscription, None);
        Ok(builder.finished_data().to_vec())
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let fbs = root_as_subscription(bytes)?;
        
        let webhooks = fbs.webhooks()
            .map(|w| w.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();
            
        let web_push_subscriptions = fbs.web_push_subscriptions()
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
            
        let f = fbs.filter();
        let filter = SubscriptionFilter {
            ids: f.ids().map(|ids| ids.iter().map(|s| s.to_string()).collect()),
            authors: f.authors().map(|authors| authors.iter().map(|s| s.to_string()).collect()),
            kinds: f.kinds().map(|kinds| kinds.iter().collect()),
            search: f.search().map(|s| s.to_string()),
            tags: f.tags()
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
        };
        
        Ok(Self {
            webhooks,
            web_push_subscriptions,
            filter,
        })
    }

    pub fn matches_event(&self, event: &Event) -> bool {
        self.filter.matches_event(event)
    }

    pub fn merge(&mut self, other: Subscription) {
        self.webhooks.extend(other.webhooks);
        
        // Deduplicate web push subscriptions based on endpoint
        for new_sub in other.web_push_subscriptions {
            if !self.web_push_subscriptions.iter().any(|existing| existing.endpoint == new_sub.endpoint) {
                self.web_push_subscriptions.push(new_sub);
            }
        }
    }

    pub fn matches_event_filter_only(bytes: &[u8], event: &Event) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let fbs = root_as_subscription(bytes)?;
        let f = fbs.filter();
        
        let filter = SubscriptionFilter {
            ids: f.ids().map(|ids| ids.iter().map(|s| s.to_string()).collect()),
            authors: f.authors().map(|authors| authors.iter().map(|s| s.to_string()).collect()),
            kinds: f.kinds().map(|kinds| kinds.iter().collect()),
            search: f.search().map(|s| s.to_string()),
            tags: f.tags()
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
        };
        
        Ok(filter.matches_event(event))
    }
}
