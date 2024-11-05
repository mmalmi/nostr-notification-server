use std::sync::Arc;
use std::time::Duration;
use nostr_sdk::nostr::{Filter, Kind, Timestamp, PublicKey};
use nostr_sdk::{Client, Keys, Options, EventSource};
use tokio::time::sleep;
use log::{info, error, warn, debug};
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::HashSet;
use std::ops::Not;

use crate::social_graph::SocialGraph;
use crate::profiles::ProfileHandler;

pub struct Crawler {
    client: Client,
    social_graph: Arc<SocialGraph>,
    profile_handler: Arc<ProfileHandler>,
    relays: Vec<String>,
    shutdown: Arc<AtomicBool>,
}

impl Crawler {
    pub fn new(
        social_graph: Arc<SocialGraph>,
        profile_handler: Arc<ProfileHandler>,
        relays: Vec<String>,
        shutdown: Arc<AtomicBool>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // Generate new keys for the client
        let keys = Keys::generate();
        
        // Create client with custom options
        let opts = Options::new()
            .wait_for_send(true)
            .connection_timeout(Some(Duration::from_secs(10)));
        let client = Client::with_opts(&keys, opts);

        Ok(Self {
            client,
            social_graph,
            profile_handler,
            relays,
            shutdown,
        })
    }

    pub async fn start(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Connect to relays
        for relay in &self.relays {
            match self.client.add_relay(relay).await {
                Ok(_) => info!("Added relay: {}", relay),
                Err(e) => error!("Failed to add relay {}: {}", relay, e),
            }
        }
        self.client.connect().await;
        info!("Connected to relays");

        // Get root pubkey
        let root = self.social_graph.get_root()?;
        
        // First fetch the root's follow list
        self.fetch_root_follows(&root).await?;
        
        // Then process missing follow lists
        self.get_missing_follow_lists(&root).await?;

        Ok(())
    }

    async fn fetch_root_follows(&self, root: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let root_key = PublicKey::from_hex(root)?;
        let filter = Filter::new()
            .authors(vec![root_key])
            .kind(Kind::ContactList)
            .limit(1);

        let events = self.client.get_events_of(
            vec![filter],
            EventSource::relays(Some(Duration::from_secs(10)))
        ).await?;

        if let Some(event) = events.first() {
            self.social_graph.handle_event(event)?;
            self.profile_handler.handle_event(event)?;
        } else {
            info!("No root follow event found");
        }

        Ok(())
    }

    async fn get_missing_follow_lists(&self, root: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let root_follows = self.social_graph.get_followed_by_user(root, false)?;
        let mut missing = root_follows.iter()
            .filter(|&pubkey| self.social_graph.get_followed_by_user(pubkey, false).unwrap_or_default().is_empty())
            .cloned()
            .collect::<Vec<String>>();

        info!("Fetching {} missing follow lists", missing.len());
        let total = missing.len();
        let mut processed = 0;

        while !missing.is_empty() && !self.shutdown.load(Ordering::Relaxed) {
            let batch_size = std::cmp::min(50, missing.len());
            let batch: Vec<_> = missing.drain(..batch_size)
                .filter_map(|hex| PublicKey::from_hex(&hex).ok())
                .collect();
            
            info!("Processing batch of {} pubkeys ({}/{} total)", batch.len(), processed + batch_size, total);
            
            let filter = Filter::new()
                .authors(batch.clone())
                .kind(Kind::ContactList)
                .since(Timestamp::now() - 30 * 24 * 60 * 60);

            let events = match tokio::time::timeout(
                Duration::from_secs(5),
                self.client.get_events_of(
                    vec![filter],
                    EventSource::relays(Some(Duration::from_secs(3)))
                )
            ).await {
                Ok(Ok(events)) => {
                    info!("Received {} events for batch", events.len());
                    events
                }
                Ok(Err(e)) => {
                    error!("Error fetching events for batch: {}", e);
                    missing.extend(batch.into_iter().map(|pk| pk.to_hex()));
                    processed += batch_size;
                    continue;
                }
                Err(_) => {
                    warn!("Timeout fetching events for batch");
                    missing.extend(batch.into_iter().map(|pk| pk.to_hex()));
                    processed += batch_size;
                    continue;
                }
            };

            debug!("Starting to process {} events", events.len());
            
            // Process events with timeout and detailed logging
            match tokio::time::timeout(
                Duration::from_secs(2),
                async {
                    for (i, event) in events.iter().enumerate() {
                        if self.shutdown.load(Ordering::Relaxed) {
                            debug!("Shutdown signal received during event processing");
                            return;
                        }
                        
                        debug!("Processing event {}/{}", i + 1, events.len());
                        let author = event.pubkey.to_hex();
                        
                        debug!("Handling social graph for event {}/{}", i + 1, events.len());
                        if let Err(e) = self.social_graph.handle_event(event) {
                            error!("Error handling follow event from {}: {}", author, e);
                        }
                        
                        debug!("Handling profile for event {}/{}", i + 1, events.len());
                        if let Err(e) = self.profile_handler.handle_event(event) {
                            error!("Error handling profile event from {}: {}", author, e);
                        }
                        
                        debug!("Finished processing event {}/{}", i + 1, events.len());
                    }
                    debug!("Finished processing all events in batch");
                }
            ).await {
                Ok(_) => info!("Successfully processed batch events"),
                Err(_) => warn!("Timeout processing batch events"),
            }

            processed += batch_size;
            info!("Processed batch. {} of {} total", processed, total);
            
            // Print stats after each batch
            let (total_users, total_follows) = self.social_graph.size()?;
            info!("Current stats:");
            info!("  Users in graph: {}", total_users);
            info!("  Total follows: {}", total_follows);

            if !missing.is_empty() && !self.shutdown.load(Ordering::Relaxed) {
                info!("Waiting before next batch... {} remaining", missing.len());
                tokio::time::timeout(
                    Duration::from_millis(100),
                    sleep(Duration::from_millis(100))
                ).await.ok();
            }
        }

        if self.shutdown.load(Ordering::Relaxed) {
            info!("Shutdown signal received, stopping follow list processing");
        } else {
            info!("Finished processing all follow lists");
        }
        Ok(())
    }

    pub async fn crawl_profiles(&self, missing_only: bool) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Connect to relays first
        for relay in &self.relays {
            match self.client.add_relay(relay).await {
                Ok(_) => info!("Added relay: {}", relay),
                Err(e) => error!("Failed to add relay {}: {}", relay, e),
            }
        }
        self.client.connect().await;
        info!("Connected to relays");

        // Get all pubkeys that need profile updates
        let mut pubkeys = if missing_only {
            self.get_missing_profiles()?
        } else {
            self.get_all_profiles()?
        };

        info!("Fetching {} profiles", pubkeys.len());
        let total = pubkeys.len();
        let mut processed = 0;

        while !pubkeys.is_empty() && !self.shutdown.load(Ordering::Relaxed) {
            let batch_size = std::cmp::min(50, pubkeys.len());
            let batch: Vec<_> = pubkeys.drain(..batch_size)
                .filter_map(|hex| PublicKey::from_hex(&hex).ok())
                .collect();
            
            info!("Processing batch of {} profiles ({}/{} total)", batch.len(), processed + batch_size, total);
            
            let filter = Filter::new()
                .authors(batch.clone())
                .kind(Kind::Metadata)
                .since(Timestamp::now() - 30 * 24 * 60 * 60);

            let events = match tokio::time::timeout(
                Duration::from_secs(5),
                self.client.get_events_of(
                    vec![filter],
                    EventSource::relays(Some(Duration::from_secs(3)))
                )
            ).await {
                Ok(Ok(events)) => events,
                Ok(Err(e)) => {
                    error!("Error fetching profiles for batch: {}", e);
                    processed += batch_size;
                    continue;
                }
                Err(_) => {
                    warn!("Timeout fetching profiles for batch");
                    processed += batch_size;
                    continue;
                }
            };

            for event in events {
                if let Err(e) = self.profile_handler.handle_event(&event) {
                    error!("Error handling profile event: {}", e);
                }
            }

            processed += batch_size;
            info!("Processed batch. {} of {} total", processed, total);
            
            // Print stats after each batch
            let profile_count = self.profile_handler.get_profile_count()?;
            let (total_users, total_follows) = self.social_graph.size()?;
            info!("Current stats:");
            info!("  Profiles stored: {}", profile_count);
            info!("  Users in graph: {}", total_users);
            info!("  Total follows: {}", total_follows);

            if !pubkeys.is_empty() && !self.shutdown.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        if self.shutdown.load(Ordering::Relaxed) {
            info!("Shutdown signal received, stopping profile crawl");
        } else {
            info!("Finished crawling all profiles");
        }
        Ok(())
    }

    fn get_missing_profiles(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        let mut pubkeys = HashSet::new();
        
        // Get all users from social graph
        let root = self.social_graph.get_root()?;
        let mut to_check = vec![root];
        
        while let Some(user) = to_check.pop() {
            pubkeys.insert(user.clone());
            let followed = self.social_graph.get_followed_by_user(&user, false)?;
            for pubkey in followed {
                if !pubkeys.contains(&pubkey) {
                    to_check.push(pubkey);
                }
            }
        }

        // Filter out users that already have profiles
        let rtxn = self.profile_handler.env.read_txn()?;
        Ok(pubkeys.into_iter()
            .filter(|pubkey| {
                self.profile_handler.has_profile_with_txn(&rtxn, pubkey)
                    .unwrap_or(false)
                    .not()
            })
            .collect())
    }

    fn get_all_profiles(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        let mut pubkeys = HashSet::new();
        
        // Get all users from social graph
        let root = self.social_graph.get_root()?;
        let mut to_check = vec![root];
        
        while let Some(user) = to_check.pop() {
            pubkeys.insert(user.clone());
            let followed = self.social_graph.get_followed_by_user(&user, false)?;
            for pubkey in followed {
                if !pubkeys.contains(&pubkey) {
                    to_check.push(pubkey);
                }
            }
        }

        Ok(pubkeys.into_iter().collect())
    }
}

