use heed::{Database, Env, EnvOpenOptions};
use heed::types::*;
use std::sync::Arc;
use std::fs;
use crate::subscription::Subscription;
use serde::{Serialize, Deserialize};
use bincode;
use log::debug;
use crate::config::Settings;

#[derive(Serialize, Deserialize)]
pub struct PTagIndex {
    pub pubkeys: Vec<String>
}

impl PTagIndex {
    pub fn new(pubkeys: Vec<String>) -> Self {
        Self { pubkeys }
    }

    pub fn serialize(&self) -> Vec<u8> {
        bincode::serialize(self).unwrap()
    }

    pub fn deserialize(bytes: &[u8]) -> Option<Self> {
        bincode::deserialize(bytes).ok()
    }
}

#[derive(Clone)]
pub struct DbHandler {
    env: Arc<Env>,
    db: Arc<Database<Str, Bytes>>,
    subscriptions_by_p_tag: Arc<Database<Str, Bytes>>,
}

impl DbHandler {
    pub fn new(settings: &Settings) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        fs::create_dir_all(&settings.db_path)?;

        let environment = Arc::new(unsafe {
            EnvOpenOptions::new()
                .map_size(settings.db_map_size)
                .max_dbs(10)
                .open(&settings.db_path)?
        });

        let (db, subscriptions_by_p_tag) = {
            let mut wtxn = environment.write_txn()?;
            let db = environment.create_database(&mut wtxn, Some("subscriptions"))?;
            let subscriptions_by_p_tag = environment.create_database(&mut wtxn, Some("subscriptions_by_p_tag"))?;
            wtxn.commit()?;
            (db, subscriptions_by_p_tag)
        };

        Ok(Self {
            env: environment,
            db: Arc::new(db),
            subscriptions_by_p_tag: Arc::new(subscriptions_by_p_tag),
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
                let mut index = self.subscriptions_by_p_tag
                    .get(&wtxn, p_value)?
                    .and_then(|bytes| PTagIndex::deserialize(bytes))
                    .unwrap_or_else(|| PTagIndex::new(Vec::new()));
                
                if !index.pubkeys.contains(&pubkey.to_string()) {
                    index.pubkeys.push(pubkey.to_string());
                    self.subscriptions_by_p_tag.put(&mut wtxn, p_value, &index.serialize())?;
                    debug!("Added pubkey {} to p tag index for {}", pubkey, p_value);
                }
            }
        }
        
        wtxn.commit()?;
        Ok(())
    }

    pub fn get_env(&self) -> Arc<Env> {
        Arc::clone(&self.env)
    }

    pub fn get_db(&self) -> Arc<Database<Str, Bytes>> {
        Arc::clone(&self.db)
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

    pub fn get_p_tag_index(&self, p_value: &str) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        let result = self.subscriptions_by_p_tag.get(&rtxn, p_value)?.map(|bytes| bytes.to_vec());
        debug!("Retrieved p tag index for {}: {:?}", p_value, result.is_some());
        Ok(result)
    }
}
