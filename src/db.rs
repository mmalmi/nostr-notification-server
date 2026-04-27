use crate::config::Settings;
use crate::external_social_graph::ExternalSocialGraph;
use crate::subscription::Subscription;
use heed::byteorder::BigEndian;
use heed::types::*;
use heed::{Database, Env, EnvOpenOptions};
use log::{debug, warn};
use nostr_sdk::Event;
use nostr_sdk::{FromBech32, PublicKey};
use nostr_social_graph::{ProfileHandler, SerializedSocialGraph, SocialGraph};
use std::collections::HashSet;
use std::error::Error as StdError;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct DbHandler {
    pub env: Env,
    pub subscriptions: Database<Str, Bytes>,
    subscriptions_by_p_tag_and_id: Database<Str, Str>,
    subscriptions_by_pubkey_and_id: Database<Str, Str>,
    subscriptions_by_author_and_id: Database<Str, Str>,
    pub social_graph: SocialGraph,
    pub profiles: ProfileHandler,
    metadata: Database<Str, Bytes>,
    seen_events: Database<Str, U8>,
    recipient_muted_pubkeys: Database<Str, SerdeBincode<HashSet<String>>>,
    recipient_mute_list_created_at: Database<Str, U64<BigEndian>>,
    push_target_last_sent_at: Database<Str, U64<BigEndian>>,
    external_social_graph: Option<ExternalSocialGraph>,
}

impl DbHandler {
    pub fn new(settings: &Settings) -> Result<Self, Box<dyn StdError + Send + Sync>> {
        fs::create_dir_all(&settings.db_path)?;

        let environment = unsafe {
            EnvOpenOptions::new()
                .map_size(settings.db_map_size)
                .max_dbs(20)
                .open(&settings.db_path)?
        };

        let (
            subscriptions,
            subscriptions_by_p_tag_and_id,
            subscriptions_by_pubkey_and_id,
            subscriptions_by_author_and_id,
            metadata,
            seen_events,
            recipient_muted_pubkeys,
            recipient_mute_list_created_at,
            push_target_last_sent_at,
        ) = {
            let mut wtxn = environment.write_txn()?;
            let subscriptions = environment.create_database(&mut wtxn, Some("subscriptions"))?;
            let subscriptions_by_p_tag_and_id =
                environment.create_database(&mut wtxn, Some("subscriptions_by_p_tag_and_id"))?;
            let subscriptions_by_pubkey_and_id =
                environment.create_database(&mut wtxn, Some("subscriptions_by_pubkey_and_id"))?;
            let subscriptions_by_author_and_id =
                environment.create_database(&mut wtxn, Some("subscriptions_by_author_and_id"))?;
            let metadata = environment.create_database(&mut wtxn, Some("metadata"))?;
            let seen_events = environment.create_database(&mut wtxn, Some("seen_events"))?;
            let recipient_muted_pubkeys =
                environment.create_database(&mut wtxn, Some("recipient_muted_pubkeys"))?;
            let recipient_mute_list_created_at =
                environment.create_database(&mut wtxn, Some("recipient_mute_list_created_at"))?;
            let push_target_last_sent_at =
                environment.create_database(&mut wtxn, Some("push_target_last_sent_at"))?;
            wtxn.commit()?;
            (
                subscriptions,
                subscriptions_by_p_tag_and_id,
                subscriptions_by_pubkey_and_id,
                subscriptions_by_author_and_id,
                metadata,
                seen_events,
                recipient_muted_pubkeys,
                recipient_mute_list_created_at,
                push_target_last_sent_at,
            )
        };

        let root_pubkey = &settings.social_graph_root_pubkey;
        let root_hex = if root_pubkey.starts_with("npub") {
            PublicKey::from_bech32(root_pubkey)
                .map_err(|e| {
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        e.to_string(),
                    )) as Box<dyn StdError + Send + Sync>
                })?
                .to_hex()
        } else {
            root_pubkey.to_string()
        };

        let social_graph = SocialGraph::new(
            &root_hex,
            environment.clone(),
            None as Option<SerializedSocialGraph>,
        )
        .map_err(|e| {
            let err_string = e.to_string();
            Box::new(std::io::Error::new(std::io::ErrorKind::Other, err_string))
                as Box<dyn StdError + Send + Sync>
        })?;

        let profiles = ProfileHandler::new(environment.clone()).map_err(|e| {
            let err_string = e.to_string();
            Box::new(std::io::Error::new(std::io::ErrorKind::Other, err_string))
                as Box<dyn StdError + Send + Sync>
        })?;

        let external_social_graph = settings
            .social_graph_snapshot_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            .and_then(|path| match ExternalSocialGraph::open(path) {
                Ok(graph) => Some(graph),
                Err(error) => {
                    warn!(
                        "Failed to open external social graph at {}: {}. Falling back to local graph state.",
                        path, error
                    );
                    None
                }
            });

        Ok(Self {
            env: environment,
            subscriptions,
            subscriptions_by_p_tag_and_id,
            subscriptions_by_pubkey_and_id,
            subscriptions_by_author_and_id,
            social_graph,
            profiles,
            metadata,
            seen_events,
            recipient_muted_pubkeys,
            recipient_mute_list_created_at,
            push_target_last_sent_at,
            external_social_graph,
        })
    }

    pub fn get_subscriptions_for_pubkey(
        &self,
        pubkey: &str,
    ) -> Result<Vec<(String, Subscription)>, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        let prefix = format!("{}:", pubkey);

        let subscriptions: Vec<(String, Subscription)> = self
            .subscriptions_by_pubkey_and_id
            .prefix_iter(&rtxn, &prefix)?
            .filter_map(|result| {
                result.ok().and_then(|(key, _)| {
                    // Extract subscription ID from the key (format: "pubkey:id")
                    let id = key.split(':').nth(1)?;
                    self.subscriptions
                        .get(&rtxn, id)
                        .ok()?
                        .map(|bytes| Subscription::deserialize(bytes).ok())
                        .flatten()
                        .map(|sub| (id.to_string(), sub))
                })
            })
            .collect();

        Ok(subscriptions)
    }

    pub fn get_subscription(
        &self,
        pubkey: &str,
        id: &str,
    ) -> Result<Option<Subscription>, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;

        // First check if this subscription belongs to the pubkey
        let index_key = format!("{}:{}", pubkey, id);
        if !self
            .subscriptions_by_pubkey_and_id
            .get(&rtxn, &index_key)?
            .is_some()
        {
            return Ok(None);
        }

        match self.subscriptions.get(&rtxn, id)? {
            Some(bytes) => {
                let subscription = Subscription::deserialize(bytes)?;
                Ok(Some(subscription))
            }
            None => Ok(None),
        }
    }

    pub fn save_subscription(
        &self,
        pubkey: &str,
        id: &str,
        subscription: &Subscription,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut wtxn = self.env.write_txn()?;

        // For existing subscriptions, verify ownership and clean up old indices.
        if let Some(old_sub_bytes) = self.subscriptions.get(&wtxn, id)? {
            let index_key = format!("{}:{}", pubkey, id);
            if !self
                .subscriptions_by_pubkey_and_id
                .get(&wtxn, &index_key)?
                .is_some()
            {
                return Err("Subscription not found or unauthorized".into());
            }

            if let Ok(old_sub) = Subscription::deserialize(old_sub_bytes) {
                self.delete_subscription_indices(&mut wtxn, pubkey, id, &old_sub)?;
            }
        }

        let claimed_fcm_tokens: HashSet<String> = subscription
            .fcm_tokens
            .iter()
            .map(|token| token.trim())
            .filter(|token| !token.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        let claimed_apns_tokens: HashSet<String> = subscription
            .apns_tokens
            .iter()
            .map(|token| token.trim())
            .filter(|token| !token.is_empty())
            .map(ToOwned::to_owned)
            .collect();

        if !claimed_fcm_tokens.is_empty() || !claimed_apns_tokens.is_empty() {
            let mut touched = Vec::new();
            for result in self.subscriptions.iter(&wtxn)? {
                let (other_id, bytes) = result?;
                if other_id == id {
                    continue;
                }
                let mut other_sub = Subscription::deserialize(bytes)?;
                let original_fcm_len = other_sub.fcm_tokens.len();
                let original_apns_len = other_sub.apns_tokens.len();
                other_sub
                    .fcm_tokens
                    .retain(|token| !claimed_fcm_tokens.contains(token.trim()));
                other_sub
                    .apns_tokens
                    .retain(|token| !claimed_apns_tokens.contains(token.trim()));

                if other_sub.fcm_tokens.len() != original_fcm_len
                    || other_sub.apns_tokens.len() != original_apns_len
                {
                    touched.push((other_id.to_string(), other_sub));
                }
            }

            for (other_id, other_sub) in touched {
                let other_pubkey = other_sub.subscriber.clone();
                if other_sub.is_empty() {
                    self.subscriptions.delete(&mut wtxn, &other_id)?;
                    self.delete_subscription_indices(
                        &mut wtxn,
                        &other_pubkey,
                        &other_id,
                        &other_sub,
                    )?;
                    debug!(
                        "Deleted empty subscription {} after mobile push token moved to {}",
                        other_id, id
                    );
                } else {
                    let data = other_sub.serialize()?;
                    self.subscriptions.put(&mut wtxn, &other_id, &data)?;
                    debug!(
                        "Removed moved mobile push token from subscription {} while saving {}",
                        other_id, id
                    );
                }
            }
        }

        let data = subscription.serialize()?;

        // Save subscription by ID in main db.
        self.subscriptions.put(&mut wtxn, id, &data)?;
        self.put_subscription_indices(&mut wtxn, pubkey, id, subscription)?;

        debug!("Saving subscription with id: {}", id);
        wtxn.commit()?;
        Ok(())
    }

    pub fn delete_subscription(
        &self,
        pubkey: &str,
        id: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let mut wtxn = self.env.write_txn()?;
        let existing = self
            .subscriptions
            .get(&wtxn, id)?
            .and_then(|bytes| Subscription::deserialize(bytes).ok());
        let existed = self.subscriptions.delete(&mut wtxn, id)?;

        if let Some(subscription) = existing.as_ref() {
            self.delete_subscription_indices(&mut wtxn, pubkey, id, subscription)?;
        } else {
            let index_key = format!("{}:{}", pubkey, id);
            self.subscriptions_by_pubkey_and_id
                .delete(&mut wtxn, &index_key)?;
        }

        wtxn.commit()?;
        Ok(existed)
    }

    fn put_subscription_indices(
        &self,
        wtxn: &mut heed::RwTxn<'_>,
        pubkey: &str,
        id: &str,
        subscription: &Subscription,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let index_key = format!("{}:{}", pubkey, id);
        self.subscriptions_by_pubkey_and_id
            .put(wtxn, &index_key, "")?;

        // Update p-tag index
        if let Some(p_tags) = subscription.filter.tags.get("#p") {
            debug!("Found #p tags in subscription: {:?}", p_tags);
            for p_value in p_tags.iter() {
                debug!("Processing p tag value: {}", p_value);
                let index_key = format!("{}:{}", p_value, id);
                self.subscriptions_by_p_tag_and_id
                    .put(wtxn, &index_key, "")?;
                debug!("Added subscription to p tag index with key {}", index_key);
            }
        }

        // Add author indices
        if let Some(authors) = &subscription.filter.authors {
            for author in authors.iter() {
                let index_key = format!("{}:{}", author, id);
                self.subscriptions_by_author_and_id
                    .put(wtxn, &index_key, "")?;
                debug!("Added subscription to author index with key {}", index_key);
            }
        }

        Ok(())
    }

    fn delete_subscription_indices(
        &self,
        wtxn: &mut heed::RwTxn<'_>,
        pubkey: &str,
        id: &str,
        subscription: &Subscription,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let index_key = format!("{}:{}", pubkey, id);
        self.subscriptions_by_pubkey_and_id
            .delete(wtxn, &index_key)?;

        if let Some(p_tags) = subscription.filter.tags.get("#p") {
            for p_value in p_tags.iter() {
                let index_key = format!("{}:{}", p_value, id);
                self.subscriptions_by_p_tag_and_id
                    .delete(wtxn, &index_key)?;
            }
        }

        if let Some(authors) = &subscription.filter.authors {
            for author in authors.iter() {
                let index_key = format!("{}:{}", author, id);
                self.subscriptions_by_author_and_id
                    .delete(wtxn, &index_key)?;
            }
        }

        Ok(())
    }

    pub fn get_subscriptions_by_p_tag(
        &self,
        p_value: &str,
    ) -> Result<Vec<(String, Subscription)>, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        let prefix = format!("{}:", p_value);

        let subscriptions: Vec<(String, Subscription)> = self
            .subscriptions_by_p_tag_and_id
            .prefix_iter(&rtxn, &prefix)?
            .filter_map(|result| {
                result.ok().and_then(|(key, _)| {
                    // Extract subscription ID from the key (format: "p_tag:id")
                    let id = key.split(':').nth(1)?;
                    self.subscriptions
                        .get(&rtxn, id)
                        .ok()?
                        .map(|bytes| Subscription::deserialize(bytes).ok())
                        .flatten()
                        .map(|sub| (id.to_string(), sub))
                })
            })
            .collect();

        debug!(
            "Retrieved subscriptions for p tag {}: {:?}",
            p_value,
            subscriptions.len()
        );
        Ok(subscriptions)
    }

    pub fn get_stats(&self) -> Result<DbStats, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;

        let stats = DbStats {
            subscriptions: self.subscriptions.stat(&rtxn)?.entries as u64,
            social_graph: self.social_graph.size_with_txn(&rtxn)? as u64,
            profiles: self.profiles.names.stat(&rtxn)?.entries as u64,
        };

        rtxn.commit()?;
        Ok(stats)
    }

    pub fn get_subscriptions_by_author(
        &self,
        author: &str,
    ) -> Result<Vec<(String, Subscription)>, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        let prefix = format!("{}:", author);

        let subscriptions: Vec<(String, Subscription)> = self
            .subscriptions_by_author_and_id
            .prefix_iter(&rtxn, &prefix)?
            .filter_map(|result| {
                result.ok().and_then(|(key, _)| {
                    // Extract subscription ID from the key (format: "author:id")
                    let id = key.split(':').nth(1)?;
                    self.subscriptions
                        .get(&rtxn, id)
                        .ok()?
                        .map(|bytes| Subscription::deserialize(bytes).ok())
                        .flatten()
                        .map(|sub| (id.to_string(), sub))
                })
            })
            .collect();

        debug!(
            "Retrieved subscriptions for author {}: {:?}",
            author,
            subscriptions.len()
        );
        Ok(subscriptions)
    }

    pub fn save_last_event_time(
        &self,
        timestamp: u64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut wtxn = self.env.write_txn()?;
        let timestamp_bytes = timestamp.to_be_bytes();
        self.metadata
            .put(&mut wtxn, "last_event_time", &timestamp_bytes)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn get_last_event_time(
        &self,
    ) -> Result<Option<u64>, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        let result = self
            .metadata
            .get(&rtxn, "last_event_time")?
            .and_then(|bytes| {
                if bytes.len() == 8 {
                    let mut array = [0u8; 8];
                    array.copy_from_slice(bytes);
                    Some(u64::from_be_bytes(array))
                } else {
                    None
                }
            });
        Ok(result)
    }

    pub fn has_seen_event(
        &self,
        event_id: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        Ok(self.seen_events.get(&rtxn, event_id)?.is_some())
    }

    pub fn mark_event_seen(
        &self,
        event_id: &str,
        max_events: usize,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut wtxn = self.env.write_txn()?;

        // Check current count and clean up if needed
        let current_count = self.seen_events.len(&wtxn)?;
        if current_count >= max_events as u64 {
            // Remove oldest entries (FIFO cleanup - simple approach)
            let to_remove = (current_count - max_events as u64 / 2) as usize;
            let mut removed = 0;
            let mut keys_to_remove = Vec::new();

            for result in self.seen_events.iter(&wtxn)? {
                if let Ok((key, _)) = result {
                    if removed >= to_remove {
                        break;
                    }
                    keys_to_remove.push(key.to_string());
                    removed += 1;
                }
            }

            for key in keys_to_remove {
                self.seen_events.delete(&mut wtxn, &key)?;
            }
        }

        self.seen_events.put(&mut wtxn, event_id, &1u8)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn get_seen_events_count(&self) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        Ok(self.seen_events.len(&rtxn)?)
    }

    pub fn handle_mute_list_event(
        &self,
        event: &Event,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if event.kind.as_u16() != 10_000 {
            return Ok(());
        }

        let created_at = event.created_at.as_u64();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if created_at > now.saturating_add(10 * 60) {
            return Ok(());
        }

        let recipient = event.pubkey.to_hex();
        {
            let rtxn = self.env.read_txn()?;
            if let Some(existing_created_at) =
                self.recipient_mute_list_created_at.get(&rtxn, &recipient)?
            {
                if created_at <= existing_created_at {
                    return Ok(());
                }
            }
        }

        let muted_pubkeys: HashSet<String> = event
            .tags
            .iter()
            .filter_map(|tag| {
                let is_pubkey_tag = tag
                    .single_letter_tag()
                    .is_some_and(|letter| letter.as_char() == 'p');
                if !is_pubkey_tag {
                    return None;
                }

                let pubkey = tag.content()?;
                (pubkey.len() == 64 && pubkey.chars().all(|char| char.is_ascii_hexdigit()))
                    .then(|| pubkey.to_string())
            })
            .collect();

        let mut wtxn = self.env.write_txn()?;
        self.recipient_mute_list_created_at
            .put(&mut wtxn, &recipient, &created_at)?;
        if muted_pubkeys.is_empty() {
            self.recipient_muted_pubkeys.delete(&mut wtxn, &recipient)?;
        } else {
            self.recipient_muted_pubkeys
                .put(&mut wtxn, &recipient, &muted_pubkeys)?;
        }
        wtxn.commit()?;
        Ok(())
    }

    pub fn recipient_has_muted_author(
        &self,
        recipient: &str,
        author: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(external_social_graph) = &self.external_social_graph {
            if external_social_graph.recipient_has_muted_author(recipient, author)? {
                return Ok(true);
            }
        }

        let rtxn = self.env.read_txn()?;
        Ok(self
            .recipient_muted_pubkeys
            .get(&rtxn, recipient)?
            .is_some_and(|muted_pubkeys| muted_pubkeys.contains(author)))
    }

    pub fn is_pubkey_in_social_graph(
        &self,
        pubkey: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(external_social_graph) = &self.external_social_graph {
            if external_social_graph.is_pubkey_in_graph(pubkey)? {
                return Ok(true);
            }
        }

        Ok(self.social_graph.get_follow_distance(pubkey)? < 1000)
    }

    pub fn should_send_push_target(
        &self,
        target_key: &str,
        now: u64,
        cooldown_seconds: u64,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        if cooldown_seconds == 0 {
            return Ok(true);
        }

        let mut wtxn = self.env.write_txn()?;
        if let Some(last_sent_at) = self.push_target_last_sent_at.get(&wtxn, target_key)? {
            if now < last_sent_at.saturating_add(cooldown_seconds) {
                wtxn.abort();
                return Ok(false);
            }
        }

        self.push_target_last_sent_at
            .put(&mut wtxn, target_key, &now)?;
        wtxn.commit()?;
        Ok(true)
    }
}

pub struct DbStats {
    pub subscriptions: u64,
    pub social_graph: u64,
    pub profiles: u64,
}
