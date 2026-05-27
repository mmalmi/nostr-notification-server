use log::{debug, error, info, warn};
use nostr_sdk::{
    Event, Filter, Kind, RelayOptions, RelayPool, RelayPoolNotification, RelayPoolOptions,
    SubscribeOptions, Timestamp,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{timeout, MissedTickBehavior};

use crate::config::Settings;
use crate::db::DbHandler;
use crate::notifications::handle_incoming_event;

const NOTIFICATION_CHANNEL_SIZE: usize = 262_144;
const SHUTDOWN_CHECK_SECONDS: u64 = 1;
const LAST_EVENT_TIME_SAVE_INTERVAL_SECONDS: u64 = 5;

pub async fn run_nostr_client(
    db_handler: Arc<DbHandler>,
    settings: Arc<Settings>,
    shutdown_flag: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Starting run_nostr_client");

    // Add startup timestamp
    let startup_time = SystemTime::now();
    let startup_unix_time = startup_time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Load last event time from database
    let last_event_time = db_handler.get_last_event_time().unwrap_or_else(|e| {
        error!("Failed to load last event time: {}", e);
        None
    });

    if let Some(timestamp) = last_event_time {
        info!(
            "Loaded last event time: {} ({})",
            timestamp,
            Timestamp::from(timestamp)
        );
    } else {
        info!("No previous last event time found, starting fresh");
    }

    let relay_pool = RelayPool::new(
        RelayPoolOptions::new().notification_channel_size(NOTIFICATION_CHANNEL_SIZE),
    );
    info!(
        "Created relay pool with notification channel size {}",
        NOTIFICATION_CHANNEL_SIZE
    );

    // Connect to relays
    for relay_url in &settings.relays {
        match timeout(
            Duration::from_secs(5),
            relay_pool.add_relay(relay_url, RelayOptions::default()),
        )
        .await
        {
            Ok(Ok(_)) => info!("Connected to relay: {}", relay_url),
            Ok(Err(e)) => error!("Failed to connect to relay {}: {}", relay_url, e),
            Err(_) => error!("Timeout connecting to relay: {}", relay_url),
        }
    }
    relay_pool.connect(Some(Duration::from_secs(5))).await;
    info!("Connected to relays");

    // Use last event time if available, otherwise start from now for regular events
    let since_timestamp = if let Some(last_time) = last_event_time {
        // Add 1 second to avoid getting the last event again
        Timestamp::from(last_time + 1)
    } else {
        Timestamp::now()
    };

    let two_days_ago = Timestamp::now() - 172800; // 2 days = 172800 seconds
    let filters = vec![
        Filter::new().since(since_timestamp), // everything from last event time or now
        Filter::new().kind(Kind::Custom(1059)).since(two_days_ago), // gift wraps - kind 1059 from past 2 days
    ];

    info!("Subscribing to firehose events since: {}", since_timestamp);

    // Subscribe to events
    relay_pool
        .subscribe(filters, SubscribeOptions::default())
        .await?;

    // Get notification receiver
    let mut notifications = relay_pool.notifications();
    let mut shutdown_check = tokio::time::interval(Duration::from_secs(SHUTDOWN_CHECK_SECONDS));
    shutdown_check.set_missed_tick_behavior(MissedTickBehavior::Skip);
    shutdown_check.tick().await;
    let mut pending_last_event_time = None;
    let mut last_event_time_saved_at = last_event_time.unwrap_or_default();

    // Log seen events count on startup
    match db_handler.get_seen_events_count() {
        Ok(count) => info!("Loaded {} seen events from database", count),
        Err(e) => error!("Failed to get seen events count: {}", e),
    }

    // Handle incoming events
    loop {
        if shutdown_flag.load(Ordering::Relaxed) {
            break;
        }

        tokio::select! {
            notification = notifications.recv() => {
                match notification {
                    Ok(RelayPoolNotification::Event { relay_url, event, .. }) => {
                        if event.kind.as_u16() == 1060 || event.kind == Kind::Custom(1059) {
                            let event_age_secs = event_age_secs(&event);
                            if event.kind.as_u16() == 1060 || event_age_secs <= 30 {
                                info!(
                                    "Received encrypted notification candidate kind={} id={} relay={} event_age_secs={}",
                                    event.kind, event.id, relay_url, event_age_secs
                                );
                            } else {
                                debug!(
                                    "Received replayed gift wrap event kind={} id={} relay={} event_age_secs={}",
                                    event.kind, event.id, relay_url, event_age_secs
                                );
                            }
                        }

                        // Validate event timestamp matches our filter
                        // For gift wraps, check against two_days_ago; for others, check against since_timestamp
                        if event.kind == Kind::Custom(1059) {
                            if event.created_at < two_days_ago {
                                debug!(
                                    "Skipping gift wrap event older than filter: {} < {}",
                                    event.created_at, two_days_ago
                                );
                                continue;
                            }
                            // Skip gift wrap events in the first minutes
                            if let Ok(elapsed) = startup_time.elapsed() {
                                if elapsed < Duration::from_secs(60 * 2) {
                                    // Check if event timestamp is before startup time
                                    if event.created_at.as_u64() <= startup_unix_time {
                                        debug!("Skipping gift wrap event during startup period");
                                        continue;
                                    }
                                }
                            }
                        } else {
                            // For non-gift-wrap events, check against since_timestamp
                            if event.created_at < since_timestamp {
                                debug!(
                                    "Skipping event older than filter: {} < {}",
                                    event.created_at, since_timestamp
                                );
                                continue;
                            }
                        }

                        if let Err(e) = handle_event(*event, db_handler.clone(), settings.clone()).await {
                            error!("Error handling event: {}", e);
                        }

                        let current_time = current_unix_time();
                        pending_last_event_time = Some(current_time);
                        if current_time.saturating_sub(last_event_time_saved_at)
                            >= LAST_EVENT_TIME_SAVE_INTERVAL_SECONDS
                        {
                            save_last_event_time(&db_handler, current_time);
                            last_event_time_saved_at = current_time;
                            pending_last_event_time = None;
                        }
                    }
                    Ok(RelayPoolNotification::Shutdown) => {
                        warn!("Relay pool notification channel shut down");
                        break;
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(skipped)) => {
                        warn!("Nostr notification receiver lagged; skipped {} relay messages", skipped);
                    }
                    Err(RecvError::Closed) => {
                        warn!("Nostr notification channel closed");
                        break;
                    }
                }
            }
            _ = shutdown_check.tick() => {
                if shutdown_flag.load(Ordering::Relaxed) {
                    break;
                }
            }
        }
    }

    if let Some(last_event_time) = pending_last_event_time {
        save_last_event_time(&db_handler, last_event_time);
    }

    relay_pool.shutdown().await?;
    Ok(())
}

fn event_age_secs(event: &Event) -> u64 {
    current_unix_time().saturating_sub(event.created_at.as_u64())
}

fn current_unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn save_last_event_time(db_handler: &DbHandler, timestamp: u64) {
    if let Err(e) = db_handler.save_last_event_time(timestamp) {
        error!("Failed to save last event time: {}", e);
    }
}

async fn handle_event(
    event: Event,
    db_handler: Arc<DbHandler>,
    settings: Arc<Settings>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    debug!("Received event: {:?}", event);

    if let Err(e) = handle_incoming_event(&event, db_handler, settings.as_ref()).await {
        error!("Error handling event: {}", e);
    }

    Ok(())
}
