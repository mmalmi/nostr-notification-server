use crate::config::Settings;
use crate::external_social_graph::ExternalSocialGraph;
use crate::subscription::Subscription;
use crate::web_push::WebPushSubscription;
use heed::byteorder::BigEndian;
use heed::types::*;
use heed::{Database, Env, EnvOpenOptions};
use log::{debug, warn};
use nostr_sdk::Event;
use nostr_sdk::{FromBech32, PublicKey};
use nostr_social_graph::{ProfileHandler, SerializedSocialGraph, SocialGraph};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::error::Error as StdError;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct DbHandler {
    pub env: Env,
    pub subscriptions: Database<Str, Bytes>,
    subscriptions_by_p_tag_and_id: Database<Str, Str>,
    subscriptions_by_pubkey_and_id: Database<Str, Str>,
    subscriptions_by_author_and_id: Database<Str, Str>,
    subscriptions_by_web_push_endpoint_and_id: Database<Str, Str>,
    subscriptions_by_fcm_token_and_id: Database<Str, Str>,
    subscriptions_by_apns_token_and_id: Database<Str, Str>,
    pub social_graph: SocialGraph,
    pub profiles: ProfileHandler,
    metadata: Database<Str, Bytes>,
    seen_events: Database<Str, U8>,
    recipient_muted_pubkeys: Database<Str, SerdeBincode<HashSet<String>>>,
    recipient_mute_list_created_at: Database<Str, U64<BigEndian>>,
    push_target_rate_limits: Database<Str, SerdeBincode<PushTargetRateLimitState>>,
    external_social_graph: Option<ExternalSocialGraph>,
}

const PUSH_TARGET_INDEX_VERSION_KEY: &str = "push_target_indices_version";
const PUSH_TARGET_INDEX_VERSION: &[u8] = b"1";

#[derive(Clone, Copy)]
enum PushTargetKind {
    WebPush,
    Fcm,
    Apns,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct PushTargetRateLimitState {
    tokens: u32,
    last_refill_at: u64,
}

impl DbHandler {
    pub fn new(settings: &Settings) -> Result<Self, Box<dyn StdError + Send + Sync>> {
        fs::create_dir_all(&settings.db_path)?;

        let environment = unsafe {
            EnvOpenOptions::new()
                .map_size(settings.db_map_size)
                .max_dbs(32)
                .open(&settings.db_path)?
        };

        let (
            subscriptions,
            subscriptions_by_p_tag_and_id,
            subscriptions_by_pubkey_and_id,
            subscriptions_by_author_and_id,
            subscriptions_by_web_push_endpoint_and_id,
            subscriptions_by_fcm_token_and_id,
            subscriptions_by_apns_token_and_id,
            metadata,
            seen_events,
            recipient_muted_pubkeys,
            recipient_mute_list_created_at,
            push_target_rate_limits,
        ) = {
            let mut wtxn = environment.write_txn()?;
            let subscriptions = environment.create_database(&mut wtxn, Some("subscriptions"))?;
            let subscriptions_by_p_tag_and_id =
                environment.create_database(&mut wtxn, Some("subscriptions_by_p_tag_and_id"))?;
            let subscriptions_by_pubkey_and_id =
                environment.create_database(&mut wtxn, Some("subscriptions_by_pubkey_and_id"))?;
            let subscriptions_by_author_and_id =
                environment.create_database(&mut wtxn, Some("subscriptions_by_author_and_id"))?;
            let subscriptions_by_web_push_endpoint_and_id = environment
                .create_database(&mut wtxn, Some("subscriptions_by_web_push_endpoint_and_id"))?;
            let subscriptions_by_fcm_token_and_id = environment
                .create_database(&mut wtxn, Some("subscriptions_by_fcm_token_and_id"))?;
            let subscriptions_by_apns_token_and_id = environment
                .create_database(&mut wtxn, Some("subscriptions_by_apns_token_and_id"))?;
            let metadata = environment.create_database(&mut wtxn, Some("metadata"))?;
            let seen_events = environment.create_database(&mut wtxn, Some("seen_events"))?;
            let recipient_muted_pubkeys =
                environment.create_database(&mut wtxn, Some("recipient_muted_pubkeys"))?;
            let recipient_mute_list_created_at =
                environment.create_database(&mut wtxn, Some("recipient_mute_list_created_at"))?;
            let push_target_rate_limits =
                environment.create_database(&mut wtxn, Some("push_target_rate_limits"))?;
            wtxn.commit()?;
            (
                subscriptions,
                subscriptions_by_p_tag_and_id,
                subscriptions_by_pubkey_and_id,
                subscriptions_by_author_and_id,
                subscriptions_by_web_push_endpoint_and_id,
                subscriptions_by_fcm_token_and_id,
                subscriptions_by_apns_token_and_id,
                metadata,
                seen_events,
                recipient_muted_pubkeys,
                recipient_mute_list_created_at,
                push_target_rate_limits,
            )
        };

        let root_pubkey = &settings.social_graph_root_pubkey;
        let root_hex = if root_pubkey.starts_with("npub") {
            PublicKey::from_bech32(root_pubkey)
                .map_err(|e| {
                    Box::new(std::io::Error::other(e.to_string()))
                        as Box<dyn StdError + Send + Sync>
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
            Box::new(std::io::Error::other(err_string)) as Box<dyn StdError + Send + Sync>
        })?;

        let profiles = ProfileHandler::new(environment.clone()).map_err(|e| {
            let err_string = e.to_string();
            Box::new(std::io::Error::other(err_string)) as Box<dyn StdError + Send + Sync>
        })?;

        let external_social_graph = settings
            .social_graph_snapshot_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            .and_then(|path| match ExternalSocialGraph::open(path, &root_hex) {
                Ok(graph) => Some(graph),
                Err(error) => {
                    warn!(
                        "Failed to open external social graph at {}: {}. Falling back to local graph state.",
                        path, error
                    );
                    None
                }
            });

        let handler = Self {
            env: environment,
            subscriptions,
            subscriptions_by_p_tag_and_id,
            subscriptions_by_pubkey_and_id,
            subscriptions_by_author_and_id,
            subscriptions_by_web_push_endpoint_and_id,
            subscriptions_by_fcm_token_and_id,
            subscriptions_by_apns_token_and_id,
            social_graph,
            profiles,
            metadata,
            seen_events,
            recipient_muted_pubkeys,
            recipient_mute_list_created_at,
            push_target_rate_limits,
            external_social_graph,
        };

        handler.ensure_push_target_indices()?;
        Ok(handler)
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
                        .and_then(|bytes| Subscription::deserialize(bytes).ok())
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
        if self
            .subscriptions_by_pubkey_and_id
            .get(&rtxn, &index_key)?
            .is_none()
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
            if self
                .subscriptions_by_pubkey_and_id
                .get(&wtxn, &index_key)?
                .is_none()
            {
                return Err("Subscription not found or unauthorized".into());
            }

            if let Ok(old_sub) = Subscription::deserialize(old_sub_bytes) {
                self.delete_subscription_indices(&mut wtxn, pubkey, id, &old_sub)?;
            }
        }

        let mut stored_subscription = subscription.clone();
        normalize_web_push_subscription_list(&mut stored_subscription.web_push_subscriptions);
        normalize_push_token_list(&mut stored_subscription.fcm_tokens);
        normalize_push_token_list(&mut stored_subscription.apns_tokens);

        let claimed_web_push_endpoints =
            normalized_web_push_endpoints(&stored_subscription.web_push_subscriptions);
        let claimed_fcm_tokens = normalized_push_tokens(&stored_subscription.fcm_tokens);
        let claimed_apns_tokens = normalized_push_tokens(&stored_subscription.apns_tokens);

        if !claimed_web_push_endpoints.is_empty()
            || !claimed_fcm_tokens.is_empty()
            || !claimed_apns_tokens.is_empty()
        {
            self.evict_push_target_owners(
                &mut wtxn,
                id,
                &claimed_web_push_endpoints,
                &claimed_fcm_tokens,
                &claimed_apns_tokens,
            )?;
        }

        let data = stored_subscription.serialize()?;

        // Save subscription by ID in main db.
        self.subscriptions.put(&mut wtxn, id, &data)?;
        self.put_subscription_indices(&mut wtxn, pubkey, id, &stored_subscription)?;

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

        self.put_push_target_indices(wtxn, id, subscription)?;

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

        self.delete_push_target_indices(wtxn, id, subscription)?;

        Ok(())
    }

    fn push_target_database(&self, kind: PushTargetKind) -> &Database<Str, Str> {
        match kind {
            PushTargetKind::WebPush => &self.subscriptions_by_web_push_endpoint_and_id,
            PushTargetKind::Fcm => &self.subscriptions_by_fcm_token_and_id,
            PushTargetKind::Apns => &self.subscriptions_by_apns_token_and_id,
        }
    }

    fn put_push_target_indices(
        &self,
        wtxn: &mut heed::RwTxn<'_>,
        id: &str,
        subscription: &Subscription,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        for endpoint in normalized_web_push_endpoints(&subscription.web_push_subscriptions) {
            let index_key = push_target_index_key(&endpoint, id);
            self.subscriptions_by_web_push_endpoint_and_id
                .put(wtxn, &index_key, "")?;
        }

        for token in normalized_push_tokens(&subscription.fcm_tokens) {
            let index_key = push_target_index_key(&token, id);
            self.subscriptions_by_fcm_token_and_id
                .put(wtxn, &index_key, "")?;
        }

        for token in normalized_push_tokens(&subscription.apns_tokens) {
            let index_key = push_target_index_key(&token, id);
            self.subscriptions_by_apns_token_and_id
                .put(wtxn, &index_key, "")?;
        }

        Ok(())
    }

    fn delete_push_target_indices(
        &self,
        wtxn: &mut heed::RwTxn<'_>,
        id: &str,
        subscription: &Subscription,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        for endpoint in normalized_web_push_endpoints(&subscription.web_push_subscriptions) {
            let index_key = push_target_index_key(&endpoint, id);
            self.subscriptions_by_web_push_endpoint_and_id
                .delete(wtxn, &index_key)?;
        }

        for token in normalized_push_tokens(&subscription.fcm_tokens) {
            let index_key = push_target_index_key(&token, id);
            self.subscriptions_by_fcm_token_and_id
                .delete(wtxn, &index_key)?;
        }

        for token in normalized_push_tokens(&subscription.apns_tokens) {
            let index_key = push_target_index_key(&token, id);
            self.subscriptions_by_apns_token_and_id
                .delete(wtxn, &index_key)?;
        }

        Ok(())
    }

    fn subscription_ids_for_push_targets(
        &self,
        wtxn: &heed::RwTxn<'_>,
        kind: PushTargetKind,
        targets: &HashSet<String>,
    ) -> Result<HashSet<String>, Box<dyn std::error::Error + Send + Sync>> {
        let mut ids = HashSet::new();
        let db = self.push_target_database(kind);

        for target in targets {
            let prefix = push_target_index_prefix(target);
            for result in db.prefix_iter(wtxn, &prefix)? {
                let (key, _) = result?;
                if let Some(id) = key.strip_prefix(&prefix) {
                    ids.insert(id.to_string());
                }
            }
        }

        Ok(ids)
    }

    fn evict_push_target_owners(
        &self,
        wtxn: &mut heed::RwTxn<'_>,
        current_id: &str,
        claimed_web_push_endpoints: &HashSet<String>,
        claimed_fcm_tokens: &HashSet<String>,
        claimed_apns_tokens: &HashSet<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut touched_ids = self.subscription_ids_for_push_targets(
            wtxn,
            PushTargetKind::WebPush,
            claimed_web_push_endpoints,
        )?;
        touched_ids.extend(self.subscription_ids_for_push_targets(
            wtxn,
            PushTargetKind::Fcm,
            claimed_fcm_tokens,
        )?);
        touched_ids.extend(self.subscription_ids_for_push_targets(
            wtxn,
            PushTargetKind::Apns,
            claimed_apns_tokens,
        )?);
        touched_ids.remove(current_id);

        for other_id in touched_ids {
            let Some(bytes) = self.subscriptions.get(wtxn, &other_id)? else {
                self.delete_push_target_index_keys(
                    wtxn,
                    &other_id,
                    claimed_web_push_endpoints,
                    claimed_fcm_tokens,
                    claimed_apns_tokens,
                )?;
                continue;
            };

            let original_sub = Subscription::deserialize(bytes)?;
            let mut other_sub = original_sub.clone();
            let mut web_push_changed =
                normalize_web_push_subscription_list(&mut other_sub.web_push_subscriptions);
            web_push_changed |= retain_web_push_endpoints_not_in_set(
                &mut other_sub.web_push_subscriptions,
                claimed_web_push_endpoints,
            );
            let mut fcm_changed = normalize_push_token_list(&mut other_sub.fcm_tokens);
            fcm_changed |= retain_tokens_not_in_set(&mut other_sub.fcm_tokens, claimed_fcm_tokens);
            let mut apns_changed = normalize_push_token_list(&mut other_sub.apns_tokens);
            apns_changed |=
                retain_tokens_not_in_set(&mut other_sub.apns_tokens, claimed_apns_tokens);

            if !web_push_changed && !fcm_changed && !apns_changed {
                self.delete_push_target_index_keys(
                    wtxn,
                    &other_id,
                    claimed_web_push_endpoints,
                    claimed_fcm_tokens,
                    claimed_apns_tokens,
                )?;
                continue;
            }

            self.delete_subscription_indices(
                wtxn,
                &original_sub.subscriber,
                &other_id,
                &original_sub,
            )?;
            self.delete_push_target_indices(wtxn, &other_id, &original_sub)?;

            if other_sub.is_empty() {
                self.subscriptions.delete(wtxn, &other_id)?;
                debug!(
                    "Deleted empty subscription {} after push target moved to {}",
                    other_id, current_id
                );
            } else {
                let data = other_sub.serialize()?;
                self.subscriptions.put(wtxn, &other_id, &data)?;
                self.put_subscription_indices(wtxn, &other_sub.subscriber, &other_id, &other_sub)?;
                self.put_push_target_indices(wtxn, &other_id, &other_sub)?;
                debug!(
                    "Removed moved push target from subscription {} while saving {}",
                    other_id, current_id
                );
            }
        }

        Ok(())
    }

    fn delete_push_target_index_keys(
        &self,
        wtxn: &mut heed::RwTxn<'_>,
        id: &str,
        web_push_endpoints: &HashSet<String>,
        fcm_tokens: &HashSet<String>,
        apns_tokens: &HashSet<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        for endpoint in web_push_endpoints {
            let index_key = push_target_index_key(endpoint, id);
            self.subscriptions_by_web_push_endpoint_and_id
                .delete(wtxn, &index_key)?;
        }
        for token in fcm_tokens {
            let index_key = push_target_index_key(token, id);
            self.subscriptions_by_fcm_token_and_id
                .delete(wtxn, &index_key)?;
        }
        for token in apns_tokens {
            let index_key = push_target_index_key(token, id);
            self.subscriptions_by_apns_token_and_id
                .delete(wtxn, &index_key)?;
        }
        Ok(())
    }

    fn ensure_push_target_indices(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        {
            let rtxn = self.env.read_txn()?;
            if self
                .metadata
                .get(&rtxn, PUSH_TARGET_INDEX_VERSION_KEY)?
                .is_some_and(|version| version == PUSH_TARGET_INDEX_VERSION)
            {
                return Ok(());
            }
        }

        self.rebuild_push_target_indices()
    }

    fn rebuild_push_target_indices(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let subscriptions = {
            let rtxn = self.env.read_txn()?;
            let mut subscriptions = Vec::new();
            for result in self.subscriptions.iter(&rtxn)? {
                let (id, bytes) = result?;
                match Subscription::deserialize(bytes) {
                    Ok(subscription) => subscriptions.push((id.to_string(), subscription)),
                    Err(error) => warn!(
                        "Skipping subscription {} during push target index rebuild: {}",
                        id, error
                    ),
                }
            }
            subscriptions
        };

        let mut owner_by_web_push_endpoint = HashMap::new();
        let mut owner_by_fcm_token = HashMap::new();
        let mut owner_by_apns_token = HashMap::new();
        let mut wtxn = self.env.write_txn()?;
        self.subscriptions_by_web_push_endpoint_and_id
            .clear(&mut wtxn)?;
        self.subscriptions_by_fcm_token_and_id.clear(&mut wtxn)?;
        self.subscriptions_by_apns_token_and_id.clear(&mut wtxn)?;

        for (id, original_subscription) in subscriptions {
            let mut subscription = original_subscription.clone();
            let mut changed =
                normalize_web_push_subscription_list(&mut subscription.web_push_subscriptions);
            changed |= normalize_push_token_list(&mut subscription.fcm_tokens);
            changed |= normalize_push_token_list(&mut subscription.apns_tokens);
            changed |= retain_first_web_push_endpoint_owner(
                &mut subscription.web_push_subscriptions,
                &mut owner_by_web_push_endpoint,
                &id,
            );
            changed |= retain_first_push_token_owner(
                &mut subscription.fcm_tokens,
                &mut owner_by_fcm_token,
                &id,
            );
            changed |= retain_first_push_token_owner(
                &mut subscription.apns_tokens,
                &mut owner_by_apns_token,
                &id,
            );

            if changed && subscription.is_empty() {
                self.subscriptions.delete(&mut wtxn, &id)?;
                self.delete_subscription_indices(
                    &mut wtxn,
                    &original_subscription.subscriber,
                    &id,
                    &original_subscription,
                )?;
                debug!(
                    "Deleted empty subscription {} while rebuilding push target indices",
                    id
                );
                continue;
            }

            if changed {
                let data = subscription.serialize()?;
                self.subscriptions.put(&mut wtxn, &id, &data)?;
            }

            self.put_subscription_indices(&mut wtxn, &subscription.subscriber, &id, &subscription)?;
        }

        self.metadata.put(
            &mut wtxn,
            PUSH_TARGET_INDEX_VERSION_KEY,
            PUSH_TARGET_INDEX_VERSION,
        )?;
        wtxn.commit()?;
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
                        .and_then(|bytes| Subscription::deserialize(bytes).ok())
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
                        .and_then(|bytes| Subscription::deserialize(bytes).ok())
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
            let mut keys_to_remove = Vec::new();

            for (removed, (key, _)) in self.seen_events.iter(&wtxn)?.flatten().enumerate() {
                if removed >= to_remove {
                    break;
                }
                keys_to_remove.push(key.to_string());
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
        burst_capacity: u32,
        refill_interval_seconds: u64,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        if burst_capacity == 0 || refill_interval_seconds == 0 {
            return Ok(true);
        }

        let mut wtxn = self.env.write_txn()?;
        let mut state = self
            .push_target_rate_limits
            .get(&wtxn, target_key)?
            .unwrap_or(PushTargetRateLimitState {
                tokens: burst_capacity,
                last_refill_at: now,
            });

        state.tokens = state.tokens.min(burst_capacity);
        if state.last_refill_at > now {
            state.last_refill_at = now;
        }

        let elapsed = now.saturating_sub(state.last_refill_at);
        let refill_count = elapsed / refill_interval_seconds;
        if refill_count > 0 {
            let added_tokens = u32::try_from(refill_count).unwrap_or(u32::MAX);
            state.tokens = state
                .tokens
                .saturating_add(added_tokens)
                .min(burst_capacity);
            state.last_refill_at = if state.tokens == burst_capacity {
                now
            } else {
                state
                    .last_refill_at
                    .saturating_add(refill_count.saturating_mul(refill_interval_seconds))
            };
        }

        if state.tokens == 0 {
            self.push_target_rate_limits
                .put(&mut wtxn, target_key, &state)?;
            wtxn.commit()?;
            return Ok(false);
        }

        state.tokens -= 1;
        self.push_target_rate_limits
            .put(&mut wtxn, target_key, &state)?;
        wtxn.commit()?;
        Ok(true)
    }
}

fn normalized_web_push_endpoints(subscriptions: &[WebPushSubscription]) -> HashSet<String> {
    subscriptions
        .iter()
        .map(|subscription| subscription.endpoint.trim())
        .filter(|endpoint| !endpoint.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn normalize_web_push_subscription_list(subscriptions: &mut Vec<WebPushSubscription>) -> bool {
    let original = subscriptions.clone();
    let mut seen = HashSet::new();
    let mut normalized = Vec::with_capacity(subscriptions.len());

    for subscription in subscriptions.iter() {
        let endpoint = subscription.endpoint.trim();
        if endpoint.is_empty() || !seen.insert(endpoint.to_string()) {
            continue;
        }

        let mut normalized_subscription = subscription.clone();
        normalized_subscription.endpoint = endpoint.to_string();
        normalized.push(normalized_subscription);
    }

    let changed = original != normalized;
    if changed {
        *subscriptions = normalized;
    }
    changed
}

fn normalized_push_tokens(tokens: &[String]) -> HashSet<String> {
    tokens
        .iter()
        .map(|token| token.trim())
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn normalize_push_token_list(tokens: &mut Vec<String>) -> bool {
    let original = tokens.clone();
    let mut seen = HashSet::new();
    let mut normalized = Vec::with_capacity(tokens.len());

    for token in tokens.iter() {
        let token = token.trim();
        if !token.is_empty() && seen.insert(token.to_string()) {
            normalized.push(token.to_string());
        }
    }

    let changed = original != normalized;
    if changed {
        *tokens = normalized;
    }
    changed
}

fn retain_web_push_endpoints_not_in_set(
    subscriptions: &mut Vec<WebPushSubscription>,
    endpoints_to_remove: &HashSet<String>,
) -> bool {
    if endpoints_to_remove.is_empty() {
        return false;
    }

    let original_len = subscriptions.len();
    subscriptions
        .retain(|subscription| !endpoints_to_remove.contains(subscription.endpoint.trim()));
    original_len != subscriptions.len()
}

fn retain_tokens_not_in_set(tokens: &mut Vec<String>, tokens_to_remove: &HashSet<String>) -> bool {
    if tokens_to_remove.is_empty() {
        return false;
    }

    let original_len = tokens.len();
    tokens.retain(|token| !tokens_to_remove.contains(token.trim()));
    original_len != tokens.len()
}

fn retain_first_web_push_endpoint_owner(
    subscriptions: &mut Vec<WebPushSubscription>,
    owner_by_endpoint: &mut HashMap<String, String>,
    id: &str,
) -> bool {
    let original = subscriptions.clone();
    subscriptions.retain(
        |subscription| match owner_by_endpoint.get(&subscription.endpoint) {
            Some(owner_id) => owner_id == id,
            None => {
                owner_by_endpoint.insert(subscription.endpoint.clone(), id.to_string());
                true
            }
        },
    );
    original != *subscriptions
}

fn retain_first_push_token_owner(
    tokens: &mut Vec<String>,
    owner_by_token: &mut HashMap<String, String>,
    id: &str,
) -> bool {
    let original = tokens.clone();
    tokens.retain(|token| match owner_by_token.get(token) {
        Some(owner_id) => owner_id == id,
        None => {
            owner_by_token.insert(token.clone(), id.to_string());
            true
        }
    });
    original != *tokens
}

fn push_target_index_prefix(target: &str) -> String {
    format!("{}:", encode_push_target_index_component(target.trim()))
}

fn push_target_index_key(target: &str, id: &str) -> String {
    format!("{}{}", push_target_index_prefix(target), id)
}

fn encode_push_target_index_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value.as_bytes() {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

pub struct DbStats {
    pub subscriptions: u64,
    pub social_graph: u64,
    pub profiles: u64,
}
