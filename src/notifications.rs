use nostr_sdk::nostr::Event;
use nostr_sdk::{Kind, ToBech32};
use std::error::Error;
use std::sync::Arc;
use log::{debug, error};
use crate::config::Settings;
use crate::db::DbHandler;
use crate::subscription::Subscription;
use crate::web_push::send_web_push;
use std::time::Instant;
use serde::Serialize;

#[derive(Serialize, Debug, Clone)]
pub struct NotificationPayload {
    pub event: Option<Event>,
    pub title: String,
    pub body: String,
    pub icon: String,
    pub url: String,
}

#[derive(Serialize, Debug, Clone)]
pub struct Author {
    pub name: Option<String>,
    pub picture: Option<String>,
}

pub async fn create_notification_payload(
    event: &Event, 
    settings: &Settings,
    db_handler: &Arc<DbHandler>,
) -> NotificationPayload {
    const MAX_BODY_LENGTH: usize = 140;
    
    let event_type = match event.kind.as_u16() {
        1 => "Mention",
        4 | 1059 => "DM",
        6 => "Repost",
        7 => "Reaction",
        _ => "Notification",
    };

    let pubkey = event.pubkey.to_hex();
    let author_name = db_handler.profiles.get_name(&pubkey)
        .ok()
        .flatten()
        .unwrap_or_else(|| "Unknown".to_string());
    
    let title = format!("New {} from {}", event_type, author_name);

    let body = if event.kind.as_u16() == 1 {
        if event.content.chars().count() > MAX_BODY_LENGTH {
            format!("{}...", event.content.chars().take(MAX_BODY_LENGTH).collect::<String>())
        } else {
            event.content.clone()
        }
    } else {
        String::new()
    };

    let note_id = event.id.to_bech32().expect("Failed to convert event id to bech32");

    // Use author's profile picture as icon if available, otherwise fall back to default
    let icon = db_handler.profiles.get_picture(&pubkey)
        .ok()
        .flatten()
        .unwrap_or_else(|| settings.icon_url.clone());

    NotificationPayload {
        event: Some(event.clone()),
        title,
        body,
        icon,
        url: format!("{}/{}", settings.notification_base_url, note_id),
    }
}

async fn send_webhook(
    webhook_url: &str, 
    payload: &NotificationPayload
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let client = reqwest::Client::new();
    
    client.post(webhook_url)
        .json(&payload)
        .send()
        .await?;
    Ok(())
}

pub async fn handle_incoming_event(
    event: &Event,
    db_handler: Arc<DbHandler>,
    settings: &Settings,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let start = Instant::now();

    if event.kind == Kind::Metadata {
        db_handler.profiles.handle_event(event)?;
    } else if event.kind == Kind::ContactList {
        db_handler.social_graph.handle_event(event)?;
    }

    debug!("Processing event with kind: {} and content: {}", event.kind, 
           &event.content.chars().take(50).collect::<String>());
    
    for tag in event.tags.iter() {
        if let Some(p_value) = extract_p_tag_value(tag) {
            let tag_start = Instant::now();
            process_p_tag(p_value, event, &db_handler, settings).await?;
            debug!("Tag processing took: {:?}", tag_start.elapsed());
        }
    }
    
    debug!("Total event processing took: {:?}", start.elapsed());
    Ok(())
}

fn extract_p_tag_value(tag: &nostr_sdk::nostr::Tag) -> Option<&String> {
    let values = tag.as_slice();
    if values.get(0).map(|v| *v == "p").unwrap_or(false) {
        values.get(1)
    } else {
        None
    }
}

async fn process_p_tag(
    p_value: &String,
    event: &Event,
    db_handler: &Arc<DbHandler>,
    settings: &Settings,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    debug!("Processing p_tag: {} for event: {}", p_value, event.id);
    
    let pubkeys = db_handler.get_p_tag_index(p_value)?;
    if pubkeys.is_empty() {
        debug!("No index found for p_tag: {}", p_value);
        return Ok(());
    }
    
    debug!("Found {} subscriptions for p_tag: {}", pubkeys.len(), p_value);
    
    for pubkey in &pubkeys {
        debug!("Processing subscription for pubkey: {}", pubkey);
        process_subscription(pubkey, event, db_handler.clone(), settings).await?;
    }
    Ok(())
}

async fn process_subscription(
    pubkey: &str,
    event: &Event,
    db_handler: Arc<DbHandler>,
    settings: &Settings,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let Some(bytes) = db_handler.get_subscription_bytes(pubkey)? else { 
        debug!("No subscription found for pubkey: {}", pubkey);
        return Ok(()) 
    };
    
    if !Subscription::matches_event_filter_only(&bytes, event)? {
        debug!("Event does not match filter for pubkey: {}", pubkey);
        return Ok(());
    }

    debug!("Event matches filter for pubkey: {}", pubkey);
    let subscription = Subscription::deserialize(&bytes)?;
    let event_clone = event.clone();
    let settings_clone = settings.clone();
    let db_handler_clone = db_handler.clone();

    tokio::spawn(async move {
        if let Err(e) = send_notifications(
            subscription, 
            event_clone, 
            Arc::new(settings_clone), 
            db_handler_clone
        ).await {
            error!("Failed to send notification: {}", e);
        }
    });

    Ok(())
}

pub async fn send_notifications(
    subscription: Subscription, 
    event: Event,
    settings: Arc<Settings>,
    db_handler: Arc<DbHandler>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut tasks = Vec::new();
    let payload = create_notification_payload(&event, &settings, &db_handler).await;

    for webhook_url in subscription.webhooks {
        let payload = payload.clone();
        tasks.push(tokio::spawn(async move {
            send_webhook(&webhook_url, &payload).await
        }));
    }

    for push_sub in subscription.web_push_subscriptions {
        let payload = payload.clone();
        let settings = settings.clone();
        tasks.push(tokio::spawn(async move {
            send_web_push(&push_sub, &payload, &settings).await
        }));
    }

    for task in tasks {
        if let Err(e) = task.await? {
            error!("Notification error: {}", e);
        }
    }

    Ok(())
}
