use heed::{Database, Env, EnvOpenOptions};
use heed::types::*;
use std::fs;
use crate::subscription::Subscription;
use log::debug;
use crate::config::Settings;
use nostr_social_graph::{SocialGraph, SerializedSocialGraph, ProfileHandler};
use std::error::Error as StdError;
use nostr_sdk::{PublicKey, FromBech32};

pub struct DbHandler {
    pub env: Env,
    pub subscriptions: Database<Str, Bytes>,
    subscriptions_by_p_tag_and_id: Database<Str, Str>,
    subscriptions_by_pubkey_and_id: Database<Str, Str>,
    subscriptions_by_author_and_id: Database<Str, Str>,
    pub social_graph: SocialGraph,
    pub profiles: ProfileHandler,
    metadata: Database<Str, Bytes>,
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

        let (subscriptions, subscriptions_by_p_tag_and_id, subscriptions_by_pubkey_and_id, subscriptions_by_author_and_id, metadata) = {
            let mut wtxn = environment.write_txn()?;
            let subscriptions = environment.create_database(&mut wtxn, Some("subscriptions"))?;
            let subscriptions_by_p_tag_and_id = environment.create_database(&mut wtxn, Some("subscriptions_by_p_tag_and_id"))?;
            let subscriptions_by_pubkey_and_id = environment.create_database(&mut wtxn, Some("subscriptions_by_pubkey_and_id"))?;
            let subscriptions_by_author_and_id = environment.create_database(&mut wtxn, Some("subscriptions_by_author_and_id"))?;
            let metadata = environment.create_database(&mut wtxn, Some("metadata"))?;
            wtxn.commit()?;
            (subscriptions, subscriptions_by_p_tag_and_id, subscriptions_by_pubkey_and_id, subscriptions_by_author_and_id, metadata)
        };

        let root_pubkey = &settings.social_graph_root_pubkey;
        let root_hex = if root_pubkey.starts_with("npub") {
            PublicKey::from_bech32(root_pubkey)
                .map_err(|e| Box::new(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())) as Box<dyn StdError + Send + Sync>)?
                .to_hex()
        } else {
            root_pubkey.to_string()
        };

        let social_graph = SocialGraph::new(
            &root_hex,
            environment.clone(),
            None as Option<SerializedSocialGraph>
        ).map_err(|e| {
            let err_string = e.to_string();
            Box::new(std::io::Error::new(std::io::ErrorKind::Other, err_string)) as Box<dyn StdError + Send + Sync>
        })?;

        let profiles = ProfileHandler::new(environment.clone()).map_err(|e| {
            let err_string = e.to_string();
            Box::new(std::io::Error::new(std::io::ErrorKind::Other, err_string)) as Box<dyn StdError + Send + Sync>
        })?;

        Ok(Self {
            env: environment,
            subscriptions,
            subscriptions_by_p_tag_and_id,
            subscriptions_by_pubkey_and_id,
            subscriptions_by_author_and_id,
            social_graph,
            profiles,
            metadata,
        })
    }

    pub fn get_subscriptions_for_pubkey(&self, pubkey: &str) -> Result<Vec<(String, Subscription)>, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        let prefix = format!("{}:", pubkey);
        
        let subscriptions: Vec<(String, Subscription)> = self.subscriptions_by_pubkey_and_id
            .prefix_iter(&rtxn, &prefix)?
            .filter_map(|result| {
                result.ok().and_then(|(key, _)| {
                    // Extract subscription ID from the key (format: "pubkey:id")
                    let id = key.split(':').nth(1)?;
                    self.subscriptions.get(&rtxn, id).ok()?
                        .map(|bytes| Subscription::deserialize(bytes).ok())
                        .flatten()
                        .map(|sub| (id.to_string(), sub))
                })
            })
            .collect();

        Ok(subscriptions)
    }

    pub fn get_subscription(&self, pubkey: &str, id: &str) -> Result<Option<Subscription>, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        
        // First check if this subscription belongs to the pubkey
        let index_key = format!("{}:{}", pubkey, id);
        if !self.subscriptions_by_pubkey_and_id.get(&rtxn, &index_key)?.is_some() {
            return Ok(None);
        }
        
        match self.subscriptions.get(&rtxn, id)? {
            Some(bytes) => {
                let subscription = Subscription::deserialize(bytes)?;
                Ok(Some(subscription))
            },
            None => Ok(None),
        }
    }

    pub fn save_subscription(&self, pubkey: &str, id: &str, subscription: &Subscription) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut wtxn = self.env.write_txn()?;
        
        // For existing subscriptions, verify ownership and clean up old indices
        if self.subscriptions.get(&wtxn, id)?.is_some() {
            let index_key = format!("{}:{}", pubkey, id);
            if !self.subscriptions_by_pubkey_and_id.get(&wtxn, &index_key)?.is_some() {
                return Err("Subscription not found or unauthorized".into());
            }
            
            // Clean up old indices
            if let Ok(Some(old_sub_bytes)) = self.subscriptions.get(&wtxn, id) {
                if let Ok(old_sub) = Subscription::deserialize(old_sub_bytes) {
                    // Clean up old p-tag indices
                    if let Some(old_p_tags) = old_sub.filter.tags.get("#p") {
                        for p_value in old_p_tags.iter() {
                            let index_key = format!("{}:{}", p_value, id);
                            self.subscriptions_by_p_tag_and_id.delete(&mut wtxn, &index_key)?;
                        }
                    }
                    // Clean up old author indices
                    if let Some(authors) = old_sub.filter.authors {
                        for author in authors.iter() {
                            let index_key = format!("{}:{}", author, id);
                            self.subscriptions_by_author_and_id.delete(&mut wtxn, &index_key)?;
                        }
                    }
                }
            }
        }
        
        let data = subscription.serialize()?;
        
        // Save subscription by ID in main db
        self.subscriptions.put(&mut wtxn, id, &data)?;
        
        // Update pubkey index with composite key
        let index_key = format!("{}:{}", pubkey, id);
        self.subscriptions_by_pubkey_and_id.put(&mut wtxn, &index_key, "")?;
        
        debug!("Saving subscription with id: {}", id);
        
        // Update p-tag index
        if let Some(p_tags) = subscription.filter.tags.get("#p") {
            debug!("Found #p tags in subscription: {:?}", p_tags);
            for p_value in p_tags.iter() {
                debug!("Processing p tag value: {}", p_value);
                let index_key = format!("{}:{}", p_value, id);
                self.subscriptions_by_p_tag_and_id.put(&mut wtxn, &index_key, "")?;
                debug!("Added subscription to p tag index with key {}", index_key);
            }
        }
        
        // Add author indices
        if let Some(authors) = &subscription.filter.authors {
            for author in authors.iter() {
                let index_key = format!("{}:{}", author, id);
                self.subscriptions_by_author_and_id.put(&mut wtxn, &index_key, "")?;
                debug!("Added subscription to author index with key {}", index_key);
            }
        }
        
        wtxn.commit()?;
        Ok(())
    }

    pub fn delete_subscription(&self, pubkey: &str, id: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let mut wtxn = self.env.write_txn()?;
        let existed = self.subscriptions.delete(&mut wtxn, id)?;
        
        // Clean up indices
        let index_key = format!("{}:{}", pubkey, id);
        self.subscriptions_by_pubkey_and_id.delete(&mut wtxn, &index_key)?;
        
        wtxn.commit()?;
        Ok(existed)
    }

    pub fn get_subscriptions_by_p_tag(&self, p_value: &str) -> Result<Vec<(String, Subscription)>, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        let prefix = format!("{}:", p_value);
        
        let subscriptions: Vec<(String, Subscription)> = self.subscriptions_by_p_tag_and_id
            .prefix_iter(&rtxn, &prefix)?
            .filter_map(|result| {
                result.ok().and_then(|(key, _)| {
                    // Extract subscription ID from the key (format: "p_tag:id")
                    let id = key.split(':').nth(1)?;
                    self.subscriptions.get(&rtxn, id).ok()?
                        .map(|bytes| Subscription::deserialize(bytes).ok())
                        .flatten()
                        .map(|sub| (id.to_string(), sub))
                })
            })
            .collect();

        debug!("Retrieved subscriptions for p tag {}: {:?}", p_value, subscriptions.len());
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

    pub fn get_subscriptions_by_author(&self, author: &str) -> Result<Vec<(String, Subscription)>, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        let prefix = format!("{}:", author);
        
        let subscriptions: Vec<(String, Subscription)> = self.subscriptions_by_author_and_id
            .prefix_iter(&rtxn, &prefix)?
            .filter_map(|result| {
                result.ok().and_then(|(key, _)| {
                    // Extract subscription ID from the key (format: "author:id")
                    let id = key.split(':').nth(1)?;
                    self.subscriptions.get(&rtxn, id).ok()?
                        .map(|bytes| Subscription::deserialize(bytes).ok())
                        .flatten()
                        .map(|sub| (id.to_string(), sub))
                })
            })
            .collect();

        debug!("Retrieved subscriptions for author {}: {:?}", author, subscriptions.len());
        Ok(subscriptions)
    }

    pub fn save_last_event_time(&self, timestamp: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut wtxn = self.env.write_txn()?;
        let timestamp_bytes = timestamp.to_be_bytes();
        self.metadata.put(&mut wtxn, "last_event_time", &timestamp_bytes)?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn get_last_event_time(&self) -> Result<Option<u64>, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        let result = self.metadata.get(&rtxn, "last_event_time")?
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
}

pub struct DbStats {
    pub subscriptions: u64,
    pub social_graph: u64,
    pub profiles: u64,
}
