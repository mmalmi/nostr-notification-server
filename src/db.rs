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
    pub db: Database<Str, Bytes>,
    subscriptions_by_p_tag: Database<Str, SerdeBincode<Vec<String>>>,
    pub social_graph: SocialGraph,
    pub profiles: ProfileHandler,
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

        let (db, subscriptions_by_p_tag) = {
            let mut wtxn = environment.write_txn()?;
            let db = environment.create_database(&mut wtxn, Some("subscriptions"))?;
            let subscriptions_by_p_tag = environment.create_database(&mut wtxn, Some("subscriptions_by_p_tag"))?;
            wtxn.commit()?;
            (db, subscriptions_by_p_tag)
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
            db,
            subscriptions_by_p_tag,
            social_graph,
            profiles,
        })
    }

    pub fn get_subscription(&self, pubkey: &str) -> Result<Option<Subscription>, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        
        match self.db.get(&rtxn, pubkey)? {
            Some(bytes) => {
                let subscription = Subscription::deserialize(bytes)?;
                Ok(Some(subscription))
            },
            None => Ok(None),
        }
    }

    pub fn save_subscription(&self, pubkey: &str, subscription: &Subscription) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut wtxn = self.env.write_txn()?;
        let data = subscription.serialize()?;
        self.db.put(&mut wtxn, pubkey, &data)?;
        
        debug!("Saving subscription for pubkey: {}", pubkey);
        
        if let Some(p_tags) = subscription.filter.tags.get("#p") {
            debug!("Found #p tags in subscription: {:?}", p_tags);
            for p_value in p_tags.iter() {
                debug!("Processing p tag value: {}", p_value);
                let mut pubkeys = self.subscriptions_by_p_tag
                    .get(&wtxn, p_value)?
                    .unwrap_or_else(Vec::new);
                
                if !pubkeys.contains(&pubkey.to_string()) {
                    pubkeys.push(pubkey.to_string());
                    self.subscriptions_by_p_tag.put(&mut wtxn, p_value, &pubkeys)?;
                    debug!("Added pubkey {} to p tag index for {}", pubkey, p_value);
                }
            }
        }
        
        wtxn.commit()?;
        Ok(())
    }

    pub fn delete_subscription(&self, pubkey: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let mut wtxn = self.env.write_txn()?;
        let existed = self.db.delete(&mut wtxn, pubkey)?;
        wtxn.commit()?;
        Ok(existed)
    }

    pub fn get_subscription_bytes(&self, pubkey: &str) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        Ok(self.db.get(&rtxn, pubkey)?.map(|bytes| bytes.to_vec()))
    }

    pub fn get_p_tag_index(&self, p_value: &str) -> Result<Option<Vec<String>>, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        let result = self.subscriptions_by_p_tag.get(&rtxn, p_value)?;
        debug!("Retrieved p tag index for {}: {:?}", p_value, result.is_some());
        Ok(result)
    }

    pub fn get_stats(&self) -> Result<DbStats, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        
        let stats = DbStats {
            subscriptions: self.db.stat(&rtxn)?.entries as u64,
            social_graph: self.social_graph.size_with_txn(&rtxn)? as u64,
            profiles: self.profiles.names.stat(&rtxn)?.entries as u64,
        };

        rtxn.commit()?;
        Ok(stats)
    }
}

pub struct DbStats {
    pub subscriptions: u64,
    pub social_graph: u64,
    pub profiles: u64,
}
