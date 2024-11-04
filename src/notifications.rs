use nostr_sdk::nostr::Event;
use nostr_sdk::ToBech32;
use std::error::Error;
use std::sync::Arc;
use log::{debug, error};
use crate::config::Settings;
use crate::db::DbHandler;
use crate::subscription::Subscription;
use crate::web_push::send_web_push;
use crate::db::PTagIndex;
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

pub async fn create_notification_payload(event: &Event, settings: &Settings) -> NotificationPayload {
    const MAX_BODY_LENGTH: usize = 140;
    
    let title = match event.kind.as_u16() {
        1 => "New Nostr Mention",
        4 | 1059 => "New Direct Message",
        6 => "New Repost",
        7 => "New Reaction",
        _ => "New Nostr Notification",
    }.to_string();

    // linked post
    
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

    NotificationPayload {
        event: Some(event.clone()),
        title,
        body,
        icon: settings.icon_url.clone(),
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
    db_handler: &DbHandler,
    settings: &Settings,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let start = Instant::now();

    // print event kind and content substring
    debug!("Processing event with kind: {} and content: {}", event.kind, 
           &event.content.chars().take(50).collect::<String>());
    
    for tag in event.tags.iter() {
        if let Some(p_value) = extract_p_tag_value(tag) {
            let tag_start = Instant::now();
            process_p_tag(p_value, event, db_handler, settings).await?;
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
    db_handler: &DbHandler,
    settings: &Settings,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    debug!("Processing p_tag: {} for event: {}", p_value, event.id);
    
    let Some(index_bytes) = db_handler.get_p_tag_index(p_value)? else { 
        debug!("No index found for p_tag: {}", p_value);
        return Ok(()) 
    };
    
    let Some(index) = PTagIndex::deserialize(&index_bytes) else { 
        debug!("Failed to deserialize index for p_tag: {}", p_value);
        return Ok(()) 
    };

    debug!("Found {} subscriptions for p_tag: {}", index.pubkeys.len(), p_value);
    
    for pubkey in &index.pubkeys {
        debug!("Processing subscription for pubkey: {}", pubkey);
        process_subscription(pubkey, event, db_handler, settings).await?;
    }
    Ok(())
}

async fn process_subscription(
    pubkey: &str,
    event: &Event,
    db_handler: &DbHandler,
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

    tokio::spawn(async move {
        if let Err(e) = send_notifications(subscription, event_clone, Arc::new(settings_clone)).await {
            error!("Failed to send notification: {}", e);
        }
    });

    Ok(())
}

pub async fn send_notifications(
    subscription: Subscription, 
    event: Event,
    settings: Arc<Settings>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut tasks = Vec::new();
    let payload = create_notification_payload(&event, &settings).await;

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
