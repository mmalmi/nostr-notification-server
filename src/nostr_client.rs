use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use log::{error, info};
use nostr_sdk::{Client, Filter, Keys, RelayPoolNotification, Timestamp};
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

    let everything_filter = vec![Filter::new().since(Timestamp::now())];

    // Subscribe to events
    client.subscribe(everything_filter, None).await?;

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
            // Check if event was already seen
            let event_id = event.id.to_string();
            if seen_events.contains(&event_id) {
                continue;
            }

            // Store the event ID (using a dummy value since we only care about keys)
            seen_events.put(event_id, ());

            if let Err(e) = handle_incoming_event(&event, &db_handler, settings.as_ref()).await {
                error!("Error handling event: {}", e);
            }
        }
    }

    Ok(())
}
