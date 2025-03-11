use nostr_sdk::prelude::*;
use nostr_sdk::nostr::{Event, TagStandard};
use nostr_sdk::{Kind, ToBech32};
use std::error::Error;
use std::sync::Arc;
use log::{debug, error, info};
use crate::config::Settings;
use crate::db::DbHandler;
use crate::subscription::Subscription;
use crate::web_push::send_web_push;
use std::time::Instant;
use serde::Serialize;
use serde_json;

const MAX_REACTION_LENGTH: usize = 20;

#[derive(Serialize, Debug, Clone)]
pub struct EventDetails {
    pub id: String,
    pub author: String,
    pub kind: u16,
}

#[derive(Serialize, Debug, Clone)]
#[serde(untagged)]
pub enum EventPayload {
    Full(Event),
    Details(EventDetails),
}

#[derive(Serialize, Debug, Clone)]
pub struct NotificationPayload {
    pub event: EventPayload,
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
    let pubkey = extract_pubkey(event);
    let event_type = get_event_type(event, db_handler, &pubkey);
    let author_name = db_handler.profiles.get_name(&pubkey)
        .ok()
        .flatten()
        .unwrap_or_else(|| "Unknown".to_string());
    
    let title = create_title(&event_type, &author_name, event.kind);
    let body = create_body(event);
    let icon = get_author_icon(&pubkey, db_handler, settings);
    let event_payload = create_event_payload(event);
    let note_id = event.id.to_bech32().expect("Failed to convert event id to bech32");

    NotificationPayload {
        event: event_payload,
        title,
        body,
        icon,
        url: format!("{}/{}", settings.notification_base_url, note_id),
    }
}

fn extract_pubkey(event: &Event) -> String {
    if event.kind == Kind::ZapReceipt {
        event.tags.iter()
            .find_map(|tag| {
                if let Some(TagStandard::Description(desc)) = tag.as_standardized() {
                    Event::from_json(desc)
                        .ok()
                        .map(|zap_request| zap_request.pubkey.to_hex())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| event.pubkey.to_hex())
    } else {
        event.pubkey.to_hex()
    }
}

fn get_event_type(event: &Event, db_handler: &Arc<DbHandler>, pubkey: &str) -> String {
    match event.kind {
        Kind::TextNote => "Mention".to_string(),
        Kind::EncryptedDirectMessage | Kind::GiftWrap => "DM".to_string(),
        Kind::Repost => "Repost".to_string(),
        Kind::ZapReceipt => create_zap_message(event, db_handler, pubkey),
        Kind::Reaction => create_reaction_message(&event.content),
        _ => "Notification".to_string(),
    }
}

fn create_zap_message(event: &Event, db_handler: &Arc<DbHandler>, pubkey: &str) -> String {
    let amount = event.tags.iter()
        .find_map(|tag| {
            if let Some(TagStandard::Amount { millisats, .. }) = tag.as_standardized() {
                Some(millisats / 1000)
            } else {
                None
            }
        })
        .unwrap_or(0);

    let sender_name = db_handler.profiles.get_name(pubkey)
        .ok()
        .flatten()
        .unwrap_or_else(|| "Unknown".to_string());

    format!("{} zapped {} sats", sender_name, amount)
}

fn create_reaction_message(content: &str) -> String {
    let reaction_content = if content.chars().count() > MAX_REACTION_LENGTH {
        format!("{}...", content.chars().take(MAX_REACTION_LENGTH).collect::<String>())
    } else {
        content.to_string()
    };

    if reaction_content == "+" {
        "liked your post".to_string()
    } else {
        format!("reacted with \"{}\"", reaction_content)
    }
}

fn create_title(event_type: &str, author_name: &str, kind: Kind) -> String {
    if kind == Kind::Reaction {
        format!("{} {}", author_name, event_type)
    } else {
        format!("New {} from {}", event_type, author_name)
    }
}

fn create_body(event: &Event) -> String {
    const MAX_BODY_LENGTH: usize = 140;

    match event.kind {
        Kind::TextNote => {
            if event.content.chars().count() > MAX_BODY_LENGTH {
                format!("{}...", event.content.chars().take(MAX_BODY_LENGTH).collect::<String>())
            } else {
                event.content.clone()
            }
        },
        Kind::ZapReceipt => {
            // Extract zap comment from the description tag
            event.tags.iter()
                .find_map(|tag| {
                    if let Some(TagStandard::Description(desc)) = tag.as_standardized() {
                        Event::from_json(desc)
                            .ok()
                            .map(|zap_request| zap_request.content)
                    } else {
                        None
                    }
                })
                .unwrap_or_default()
        },
        _ => String::new()
    }
}

fn get_author_icon(pubkey: &str, db_handler: &Arc<DbHandler>, settings: &Settings) -> String {
    db_handler.profiles.get_picture(pubkey)
        .ok()
        .flatten()
        .unwrap_or_else(|| settings.icon_url.clone())
}

fn create_event_payload(event: &Event) -> EventPayload {
    match serde_json::to_vec(event) {
        Ok(serialized) if serialized.len() <= 4096 => EventPayload::Full(event.clone()),
        _ => EventPayload::Details(EventDetails {
            id: event.id.to_hex(),
            author: event.pubkey.to_hex(),
            kind: event.kind.as_u16(),
        }),
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
    } else if event.kind == Kind::ContactList && settings.use_social_graph {
        db_handler.social_graph.handle_event(event)?;
    }

    debug!("Processing event with kind: {} and content: {}", event.kind, 
           &event.content.chars().take(50).collect::<String>());
    
    // Process author subscriptions
    let author = event.pubkey.to_hex();
    process_author(&author, event, &db_handler, settings).await?;

    // Process p-tag subscriptions
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

async fn process_author(
    author: &str,
    event: &Event,
    db_handler: &Arc<DbHandler>,
    settings: &Settings,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let has_header = event.tags.iter().any(|tag| tag.as_slice().get(0).map_or(false, |v| *v == "header"));
    let should_log_info = event.kind == Kind::Replaceable(30078) && has_header;
    
    if should_log_info {
        info!("Processing author: {} for event: {}", author, event.id);
    }
    
    let subscriptions = db_handler.get_subscriptions_by_author(author)?;
    if subscriptions.is_empty() {
        if should_log_info {
            info!("No subscriptions found for author: {}", author);
        }
        return Ok(());
    }
    
    if should_log_info {
        info!("Found {} subscriptions for author: {}", subscriptions.len(), author);
    }
    
    for (subscription_id, subscription) in subscriptions {
        if should_log_info {
            info!("Processing subscription: {:?}", subscription);
        }
        if subscription.matches_event(event) {
            let event_clone = event.clone();
            let settings_clone = settings.clone();
            let db_handler_clone = db_handler.clone();
            let subscription_id = subscription_id.clone();

            tokio::spawn(async move {
                if let Err(e) = send_notifications(
                    subscription,
                    &subscription_id,  // Pass the ID here
                    event_clone, 
                    Arc::new(settings_clone), 
                    db_handler_clone
                ).await {
                    error!("Failed to send notification: {}", e);
                }
            });
        }
    }
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
    
    let subscriptions = db_handler.get_subscriptions_by_p_tag(p_value)?;
    if subscriptions.is_empty() {
        debug!("No subscriptions found for p_tag: {}", p_value);
        return Ok(());
    }
    
    debug!("Found {} subscriptions for p_tag: {}", subscriptions.len(), p_value);
    
    for (subscription_id, subscription) in subscriptions {
        debug!("Processing subscription: {:?}", subscription);
        if subscription.matches_event(event) {
            let event_clone = event.clone();
            let settings_clone = settings.clone();
            let db_handler_clone = db_handler.clone();
            let subscription_id = subscription_id.clone();

            tokio::spawn(async move {
                if let Err(e) = send_notifications(
                    subscription,
                    &subscription_id,  // Pass the ID here
                    event_clone, 
                    Arc::new(settings_clone), 
                    db_handler_clone
                ).await {
                    error!("Failed to send notification: {}", e);
                }
            });
        }
    }
    Ok(())
}

pub async fn send_notifications(
    mut subscription: Subscription, 
    subscription_id: &str,
    event: Event,
    settings: Arc<Settings>,
    db_handler: Arc<DbHandler>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut tasks = Vec::new();
    let payload = create_notification_payload(&event, &settings, &db_handler).await;

    for webhook_url in subscription.webhooks.clone() {
        let payload = payload.clone();
        tasks.push(tokio::spawn(async move {
            send_webhook(&webhook_url, &payload).await.map(|_| None)
        }));
    }

    for push_sub in subscription.web_push_subscriptions.clone() {
        let payload = payload.clone();
        let settings = settings.clone();
        tasks.push(tokio::spawn(async move {
            send_web_push(&push_sub, &payload, &settings).await.map(|should_remove| {
                if should_remove {
                    Some(push_sub.endpoint)
                } else {
                    None
                }
            })
        }));
    }

    let mut endpoints_to_remove = Vec::new();
    for task in tasks {
        match task.await? {
            Ok(Some(endpoint)) => {
                endpoints_to_remove.push(endpoint);
            }
            _ => {}
        }
    }

    if !endpoints_to_remove.is_empty() {
        subscription.web_push_subscriptions.retain(|sub| 
            !endpoints_to_remove.contains(&sub.endpoint)
        );

        if !subscription.is_empty() {
            db_handler.save_subscription(&subscription.subscriber, subscription_id, &subscription)?;
        }
    }

    if subscription.is_empty() {
        db_handler.delete_subscription(&subscription.subscriber, subscription_id)?;
    }

    Ok(())
}
