use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use log::{error, info, debug, warn};
use nostr_sdk::{Client, Filter, Keys, RelayPoolNotification, Timestamp, Event, Kind};
use tokio::time::timeout;
use clru::CLruCache;
use std::num::NonZeroUsize;

use crate::config::Settings;
use crate::db::DbHandler;
use crate::notifications::handle_incoming_event;

pub async fn run_nostr_client(
    db_handler: Arc<DbHandler>,
    settings: Arc<Settings>,
    shutdown_flag: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Starting run_nostr_client");

    // Add startup timestamp
    let startup_time = std::time::SystemTime::now();
    
    // Generate new random keys
    let my_keys = Keys::generate();
    info!("Generated new keys");

    // Create new client with default options
    let client = Client::new(&my_keys);
    info!("Created new client");

    // Connect to relays
    for relay_url in &settings.relays {
        match timeout(Duration::from_secs(5), client.add_relay(relay_url)).await {
            Ok(Ok(_)) => info!("Connected to relay: {}", relay_url),
            Ok(Err(e)) => error!("Failed to connect to relay {}: {}", relay_url, e),
            Err(_) => error!("Timeout connecting to relay: {}", relay_url),
        }
    }
    client.connect().await;
    info!("Connected to relays");

    let two_days_ago = Timestamp::now() - 172800; // 2 days = 172800 seconds
    let filters = vec![
        Filter::new().since(Timestamp::now()), // everything from now
        Filter::new().kind(Kind::Custom(1059)).since(two_days_ago), // gift wraps - kind 1059 from past 2 days
    ];

    // Subscribe to events
    client.subscribe(filters, None).await?;

    // Get notification receiver
    let mut notifications = client.notifications();

    // Create a CLruCache with capacity of 1000
    let mut seen_events = CLruCache::new(NonZeroUsize::new(1000).unwrap());

    // Handle incoming events
    while let Ok(notification) = notifications.recv().await {
        if shutdown_flag.load(Ordering::Relaxed) {
            break;
        }

        if let RelayPoolNotification::Event { event, .. } = notification {
            let event_id = event.id.to_string();
            
            if seen_events.contains(&event_id) {
                continue;
            }

            // Skip gift wrap events in the first minutes
            if event.kind == Kind::Custom(1059) {
                if let Ok(elapsed) = startup_time.elapsed() {
                    if elapsed < Duration::from_secs(60 * 2) {
                        // Check if event timestamp is before startup time
                        if event.created_at.as_u64() <= startup_time.elapsed().unwrap().as_secs() {
                            debug!("Skipping gift wrap event during startup period");
                            continue;
                        }
                    }
                }
            }

            seen_events.put(event_id, ());

            if let Err(e) = handle_event(*event, db_handler.clone(), settings.clone()).await {
                error!("Error handling event: {}", e);
            }
        }
    }

    Ok(())
}

async fn handle_event(
    event: Event,
    db_handler: Arc<DbHandler>,
    settings: Arc<Settings>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    debug!("Received event: {:?}", event);

    // Verify event signature
    if let Err(e) = event.verify() {
        warn!("Event signature verification failed: {:?}", e);
        return Ok(());
    }

    if let Err(e) = handle_incoming_event(&event, db_handler, settings.as_ref()).await {
        error!("Error handling event: {}", e);
    }

    Ok(())
}
