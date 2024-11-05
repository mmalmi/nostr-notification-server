use heed::{Database, Env};
use heed::types::*;
use heed::byteorder::BigEndian;
use serde_json;
use nostr::{Event, Kind};

pub struct ProfileHandler {
    env: Env,
    timestamps: Database<Str, U64<BigEndian>>,
    pub names: Database<Str, Str>,
}

impl ProfileHandler {
    pub fn new(env: Env) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let (names, timestamps) = {
            let mut wtxn = env.write_txn()?;
            let names = env.create_database(&mut wtxn, Some("profile_names"))?;
            let timestamps = env.create_database(&mut wtxn, Some("profile_timestamps"))?;
            wtxn.commit()?;
            (names, timestamps)
        };

        Ok(Self {
            env,
            names,
            timestamps,
        })
    }

    pub fn handle_event(&self, event: &Event) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if event.kind != Kind::Metadata || event.content.is_empty() {
            return Ok(());
        }

        let pubkey = event.pubkey.to_hex();
        let timestamp = event.created_at.as_u64();

        let mut wtxn = self.env.write_txn()?;
        if let Some(profile_timestamp) = self.timestamps.get(&wtxn, &pubkey)? {
            if timestamp > profile_timestamp {
                self.timestamps.put(&mut wtxn, &pubkey, &timestamp)?;

                if let Ok(profile_json) = serde_json::from_str::<serde_json::Value>(&event.content) {
                    if let Some(name) = profile_json.get("name").and_then(|n| n.as_str()) {
                        self.names.put(&mut wtxn, &pubkey, name)?;
                    } else if let Some(name) = profile_json.get("username").and_then(|n| n.as_str()) {
                        self.names.put(&mut wtxn, &pubkey, name)?;
                    }
                }
                
                wtxn.commit()?;
            }
        }
        
        Ok(())
    }
}
