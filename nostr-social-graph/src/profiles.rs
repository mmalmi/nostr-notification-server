use heed::{Database, Env};
use heed::types::*;
use heed::byteorder::BigEndian;
use serde_json;
use nostr_sdk::nostr::{Event, Kind};
use url::Url;
use log::debug;

pub struct ProfileHandler {
    pub(crate) env: Env,
    timestamps: Database<Str, U64<BigEndian>>,
    pub names: Database<Str, Str>,
    pub pictures: Database<Str, Str>,
}

impl ProfileHandler {
    pub fn new(env: Env) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let (names, timestamps, pictures) = {
            let mut wtxn = env.write_txn()?;
            let names = env.create_database(&mut wtxn, Some("profile_names"))?;
            let timestamps = env.create_database(&mut wtxn, Some("profile_timestamps"))?;
            let pictures = env.create_database(&mut wtxn, Some("profile_pictures"))?;
            wtxn.commit()?;
            (names, timestamps, pictures)
        };

        Ok(Self {
            env,
            names,
            timestamps,
            pictures,
        })
    }

    fn is_valid_image_url(url_str: &str) -> bool {
        let valid_extensions = [".jpg", ".jpeg", ".png", ".gif", ".webp", ".avif"];
        
        match Url::parse(url_str) {
            Ok(url) => {
                // Check scheme is http or https
                if !matches!(url.scheme(), "http" | "https") {
                    return false;
                }
                
                // Check if has a host
                if url.host_str().is_none() {
                    return false;
                }
                
                // Check file extension
                let path_lower = url.path().to_lowercase();
                valid_extensions.iter().any(|ext| path_lower.ends_with(ext))
            }
            Err(_) => false
        }
    }

    pub fn handle_event(&self, event: &Event) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if event.kind != Kind::Metadata || event.content.is_empty() {
            return Ok(());
        }

        let pubkey = event.pubkey.to_hex();
        let timestamp = event.created_at.as_u64();

        // Add debug logging for the content
        debug!("Processing profile event for {}: {}", pubkey, event.content);

        let mut wtxn = self.env.write_txn()?;
        
        // Get existing timestamp if any
        let should_update = if let Some(profile_timestamp) = self.timestamps.get(&wtxn, &pubkey)? {
            timestamp > profile_timestamp
        } else {
            // No existing profile, so we should update
            true
        };

        if should_update {
            self.timestamps.put(&mut wtxn, &pubkey, &timestamp)?;

            if let Ok(profile_json) = serde_json::from_str::<serde_json::Value>(&event.content) {
                if let Some(name) = profile_json.get("display_name").and_then(|n| n.as_str()) {
                    self.names.put(&mut wtxn, &pubkey, name)?;
                } else if let Some(name) = profile_json.get("name").and_then(|n| n.as_str()) {
                    self.names.put(&mut wtxn, &pubkey, name)?;
                } else if let Some(name) = profile_json.get("username").and_then(|n| n.as_str()) {
                    self.names.put(&mut wtxn, &pubkey, name)?;
                }
                
                if let Some(picture) = profile_json.get("picture").and_then(|p| p.as_str()) {
                    if Self::is_valid_image_url(picture) {
                        self.pictures.put(&mut wtxn, &pubkey, picture)?;
                    }
                }
            }
            
            wtxn.commit()?;
        }
        
        Ok(())
    }

    pub fn has_profile_with_txn(&self, txn: &heed::RoTxn, pubkey: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        Ok(self.names.get(txn, pubkey)?.is_some() ||
           self.pictures.get(txn, pubkey)?.is_some())
    }

    pub fn has_profile(&self, pubkey: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        self.has_profile_with_txn(&rtxn, pubkey)
    }

    pub fn get_profile_count(&self) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        Ok(self.names.stat(&rtxn)?.entries as usize)
    }

    pub fn get_name(&self, pubkey: &str) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        Ok(self.names.get(&rtxn, pubkey)?.map(|s| s.to_string()))
    }

    pub fn get_picture(&self, pubkey: &str) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
        let rtxn = self.env.read_txn()?;
        Ok(self.pictures.get(&rtxn, pubkey)?.map(|s| s.to_string()))
    }

    pub fn import_from_json(&self, json_str: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let profiles: Vec<Vec<String>> = serde_json::from_str(json_str)?;
        let mut wtxn = self.env.write_txn()?;
        let mut imported = 0;

        for profile in profiles {
            if let Some(pubkey) = profile.get(0) {
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs();

                self.timestamps.put(&mut wtxn, pubkey, &timestamp)?;

                if let Some(name) = profile.get(1) {
                    self.names.put(&mut wtxn, pubkey, name)?;
                }

                if let Some(picture) = profile.get(3) {
                    if Self::is_valid_image_url(picture) {
                        self.pictures.put(&mut wtxn, pubkey, picture)?;
                    }
                }
                
                imported += 1;
            }
        }

        wtxn.commit()?;
        Ok(imported)
    }
}
