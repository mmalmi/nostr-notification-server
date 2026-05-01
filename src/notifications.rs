use crate::config::Settings;
use crate::db::DbHandler;
use crate::mobile_push::{send_apns_push, send_fcm_push};
use crate::subscription::Subscription;
use crate::web_push::send_web_push;
use log::{debug, error, info};
use nostr_sdk::nostr::{Event, TagStandard};
use nostr_sdk::prelude::*;
use nostr_sdk::{Kind, ToBech32};
use serde::Serialize;
use serde_json;
use std::error::Error;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_REACTION_LENGTH: usize = 20;

#[derive(Serialize, Debug, Clone)]
pub struct EventDetails {
    pub id: String,
    pub author: String,
    pub kind: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

#[derive(Serialize, Debug, Clone)]
#[serde(untagged)]
pub enum EventPayload {
    Full(Box<Event>),
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
    let author_name = db_handler
        .profiles
        .get_name(&pubkey)
        .ok()
        .flatten()
        .unwrap_or_else(|| "Someone".to_string());

    let title = create_title(&event_type, &author_name, event.kind);
    let body = create_body(event);
    let icon = get_author_icon(&pubkey, db_handler, settings);
    let event_payload = create_event_payload(event);
    let note_id = event
        .id
        .to_bech32()
        .expect("Failed to convert event id to bech32");

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
        event
            .tags
            .iter()
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

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn subscription_allows_event(
    subscription: &Subscription,
    event: &Event,
    db_handler: &Arc<DbHandler>,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let sender_pubkey = extract_pubkey(event);
    if db_handler.recipient_has_muted_author(&subscription.subscriber, &sender_pubkey)? {
        debug!(
            "Skipping notification for subscription {} because subscriber muted {}",
            subscription.subscriber, sender_pubkey
        );
        return Ok(false);
    }

    if !subscription.social_graph_filter {
        return Ok(true);
    }

    let in_social_graph = db_handler.is_pubkey_in_social_graph(&sender_pubkey)?;
    if !in_social_graph {
        debug!(
            "Skipping notification for subscription {} because {} is outside the social graph",
            subscription.subscriber, sender_pubkey
        );
    }
    Ok(in_social_graph)
}

fn get_event_type(event: &Event, db_handler: &Arc<DbHandler>, pubkey: &str) -> String {
    match event.kind {
        Kind::TextNote => "Mention".to_string(),
        Kind::EncryptedDirectMessage | Kind::GiftWrap => "DM".to_string(),
        Kind::Repost => "Repost".to_string(),
        Kind::ZapReceipt => create_zap_message(event, db_handler, pubkey),
        Kind::Reaction => create_reaction_message(&event.content),
        _ if event.kind.as_u16() == 1060 => "DM".to_string(),
        _ => "Notification".to_string(),
    }
}

fn create_zap_message(event: &Event, db_handler: &Arc<DbHandler>, pubkey: &str) -> String {
    let sender_name = db_handler
        .profiles
        .get_name(pubkey)
        .ok()
        .flatten()
        .unwrap_or_else(|| "Someone".to_string());

    let amount = event.tags.iter().find_map(|tag| {
        if let Some(TagStandard::Amount { millisats, .. }) = tag.as_standardized() {
            Some(millisats)
        } else {
            None
        }
    });

    match amount {
        Some(millisats) => {
            if *millisats < 1000 {
                format!(
                    "{} zapped {:.3} sats",
                    sender_name,
                    *millisats as f64 / 1000.0
                )
            } else {
                format!("{} zapped {} sats", sender_name, millisats / 1000)
            }
        }
        None => {
            debug!("Failed to extract zap amount from event: {}", event.id);
            format!("{} sent a zap", sender_name)
        }
    }
}

fn create_reaction_message(content: &str) -> String {
    let reaction_content = if content.chars().count() > MAX_REACTION_LENGTH {
        format!(
            "{}...",
            content
                .chars()
                .take(MAX_REACTION_LENGTH)
                .collect::<String>()
        )
    } else {
        content.to_string()
    };

    if reaction_content == "+" {
        "liked your post".to_string()
    } else {
        format!("reacted {}", reaction_content)
    }
}

fn create_title(event_type: &str, author_name: &str, kind: Kind) -> String {
    if kind == Kind::Reaction || kind == Kind::ZapReceipt {
        format!("{} {}", author_name, event_type)
    } else {
        format!("{} by {}", event_type, author_name)
    }
}

fn create_body(event: &Event) -> String {
    const MAX_BODY_LENGTH: usize = 140;

    match event.kind {
        Kind::TextNote => {
            if event.content.chars().count() > MAX_BODY_LENGTH {
                format!(
                    "{}...",
                    event
                        .content
                        .chars()
                        .take(MAX_BODY_LENGTH)
                        .collect::<String>()
                )
            } else {
                event.content.clone()
            }
        }
        Kind::ZapReceipt => {
            // Extract zap comment from the description tag
            event
                .tags
                .iter()
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
        }
        _ if event.kind.as_u16() == 1060 => "New message".to_string(),
        _ => String::new(),
    }
}

fn get_author_icon(pubkey: &str, db_handler: &Arc<DbHandler>, settings: &Settings) -> String {
    db_handler
        .profiles
        .get_picture(pubkey)
        .ok()
        .flatten()
        .unwrap_or_else(|| settings.icon_url.clone())
}

fn create_event_payload(event: &Event) -> EventPayload {
    match serde_json::to_vec(event) {
        Ok(serialized) if serialized.len() <= 4096 => EventPayload::Full(Box::new(event.clone())),
        _ => EventPayload::Details(create_event_details(event)),
    }
}

fn create_event_details(event: &Event) -> EventDetails {
    let mut details = EventDetails {
        id: event.id.to_hex(),
        author: event.pubkey.to_hex(),
        kind: event.kind.as_u16(),
        created_at: None,
        tags: None,
        content: None,
        sig: None,
    };
    if event.kind.as_u16() == 1060 {
        details.created_at = Some(event.created_at.as_u64());
        details.tags = Some(header_tags(event));
        details.content = Some(event.content.clone());
        details.sig = Some(event.sig.to_string());
    }
    details
}

pub(crate) fn header_tags(event: &Event) -> Vec<Vec<String>> {
    event
        .tags
        .iter()
        .filter(|tag| {
            tag.as_slice()
                .first()
                .is_some_and(|value| value == "header")
        })
        .map(|tag| tag.clone().to_vec())
        .collect()
}

async fn send_webhook(
    webhook_url: &str,
    payload: &NotificationPayload,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let client = reqwest::Client::new();

    client.post(webhook_url).json(&payload).send().await?;
    Ok(())
}

pub async fn handle_incoming_event(
    event: &Event,
    db_handler: Arc<DbHandler>,
    settings: &Settings,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let start = Instant::now();

    // Check if we've seen this event before
    let event_id = event.id.to_string();
    match db_handler.has_seen_event(&event_id) {
        Ok(true) => {
            debug!("Event {} already seen, skipping", event_id);
            return Ok(());
        }
        Ok(false) => {
            // Mark event as seen to prevent duplicates
            if let Err(e) = db_handler.mark_event_seen(&event_id, settings.max_seen_events) {
                error!("Failed to mark event as seen: {}", e);
            }
        }
        Err(e) => {
            error!("Failed to check seen event: {}", e);
            // Continue processing to avoid missing events due to DB errors
        }
    }

    if event.kind == Kind::Metadata {
        db_handler.profiles.handle_event(event)?;
    } else if event.kind == Kind::ContactList && settings.use_social_graph {
        db_handler.social_graph.handle_event(event)?;
    } else if event.kind.as_u16() == 10_000 {
        db_handler.handle_mute_list_event(event)?;
    }

    debug!(
        "Processing event with kind: {} and content: {}",
        event.kind,
        &event.content.chars().take(50).collect::<String>()
    );

    // Process author subscriptions
    let author = event.pubkey.to_hex();
    process_author(&author, event, &db_handler, settings).await?;

    // Process p-tag subscriptions
    let p_tag_count = event
        .tags
        .iter()
        .filter(|tag| extract_p_tag_value(tag).is_some())
        .count();

    if settings.max_p_tags > 0 && p_tag_count > settings.max_p_tags {
        debug!(
            "Skipping p-tag notifications for event {} with {} p tags (max: {})",
            event.id, p_tag_count, settings.max_p_tags
        );
    } else {
        for tag in event.tags.iter() {
            if let Some(p_value) = extract_p_tag_value(tag) {
                let tag_start = Instant::now();
                process_p_tag(p_value, event, &db_handler, settings).await?;
                debug!("Tag processing took: {:?}", tag_start.elapsed());
            }
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
    let has_header = event
        .tags
        .iter()
        .any(|tag| tag.as_slice().first().is_some_and(|v| *v == "header"));
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
        info!(
            "Found {} subscriptions for author: {}",
            subscriptions.len(),
            author
        );
    }

    for (subscription_id, subscription) in subscriptions {
        if should_log_info {
            info!("Processing subscription: {:?}", subscription);
        }
        if subscription.matches_event(event)
            && subscription_allows_event(&subscription, event, db_handler)?
        {
            let event_clone = event.clone();
            let settings_clone = settings.clone();
            let db_handler_clone = db_handler.clone();
            let subscription_id = subscription_id.clone();

            tokio::spawn(async move {
                if let Err(e) = send_notifications(
                    subscription,
                    &subscription_id, // Pass the ID here
                    event_clone,
                    Arc::new(settings_clone),
                    db_handler_clone,
                )
                .await
                {
                    error!("Failed to send notification: {}", e);
                }
            });
        }
    }
    Ok(())
}

fn extract_p_tag_value(tag: &nostr_sdk::nostr::Tag) -> Option<&String> {
    let values = tag.as_slice();
    if values.first().map(|v| *v == "p").unwrap_or(false) {
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

    debug!(
        "Found {} subscriptions for p_tag: {}",
        subscriptions.len(),
        p_value
    );

    for (subscription_id, subscription) in subscriptions {
        debug!("Processing subscription: {:?}", subscription);
        if subscription.matches_event(event)
            && subscription_allows_event(&subscription, event, db_handler)?
        {
            let event_clone = event.clone();
            let settings_clone = settings.clone();
            let db_handler_clone = db_handler.clone();
            let subscription_id = subscription_id.clone();

            tokio::spawn(async move {
                if let Err(e) = send_notifications(
                    subscription,
                    &subscription_id, // Pass the ID here
                    event_clone,
                    Arc::new(settings_clone),
                    db_handler_clone,
                )
                .await
                {
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
    let now = current_unix_timestamp();

    for webhook_url in subscription.webhooks.clone() {
        let payload = payload.clone();
        tasks.push(tokio::spawn(async move {
            send_webhook(&webhook_url, &payload)
                .await
                .map(|_| NotificationTargetRemoval::default())
        }));
    }

    for push_sub in subscription.web_push_subscriptions.clone() {
        let target_key = format!("web_push:{}", push_sub.endpoint);
        if !db_handler.should_send_push_target(
            &target_key,
            now,
            settings.push_rate_limit_burst,
            settings.push_rate_limit_refill_seconds,
        )? {
            debug!(
                "Skipping web push target due to rate limit: {}",
                push_sub.endpoint
            );
            continue;
        }

        let payload = payload.clone();
        let settings = settings.clone();
        tasks.push(tokio::spawn(async move {
            send_web_push(&push_sub, &payload, &settings)
                .await
                .map(|should_remove| NotificationTargetRemoval {
                    web_push_endpoint: should_remove.then_some(push_sub.endpoint),
                    fcm_token: None,
                    apns_token: None,
                })
        }));
    }

    for fcm_token in subscription.fcm_tokens.clone() {
        let target_key = format!("fcm:{fcm_token}");
        if !db_handler.should_send_push_target(
            &target_key,
            now,
            settings.push_rate_limit_burst,
            settings.push_rate_limit_refill_seconds,
        )? {
            debug!("Skipping FCM target due to rate limit");
            continue;
        }

        let payload = payload.clone();
        let settings = settings.clone();
        tasks.push(tokio::spawn(async move {
            send_fcm_push(&fcm_token, &payload, &settings)
                .await
                .map(|should_remove| NotificationTargetRemoval {
                    web_push_endpoint: None,
                    fcm_token: should_remove.then_some(fcm_token),
                    apns_token: None,
                })
        }));
    }

    for apns_token in subscription.apns_tokens.clone() {
        let target_key = format!("apns:{apns_token}");
        if !db_handler.should_send_push_target(
            &target_key,
            now,
            settings.push_rate_limit_burst,
            settings.push_rate_limit_refill_seconds,
        )? {
            debug!("Skipping APNS target due to rate limit");
            continue;
        }

        let payload = payload.clone();
        let settings = settings.clone();
        tasks.push(tokio::spawn(async move {
            send_apns_push(&apns_token, &payload, &settings)
                .await
                .map(|should_remove| NotificationTargetRemoval {
                    web_push_endpoint: None,
                    fcm_token: None,
                    apns_token: should_remove.then_some(apns_token),
                })
        }));
    }

    let mut endpoints_to_remove = Vec::new();
    let mut fcm_tokens_to_remove = Vec::new();
    let mut apns_tokens_to_remove = Vec::new();
    for task in tasks {
        match task.await? {
            Ok(removal) => {
                if let Some(endpoint) = removal.web_push_endpoint {
                    endpoints_to_remove.push(endpoint);
                }
                if let Some(token) = removal.fcm_token {
                    fcm_tokens_to_remove.push(token);
                }
                if let Some(token) = removal.apns_token {
                    apns_tokens_to_remove.push(token);
                }
            }
            Err(error) => {
                error!(
                    "Notification delivery task failed: {}",
                    describe_error_chain(error.as_ref())
                );
            }
        }
    }

    if !endpoints_to_remove.is_empty()
        || !fcm_tokens_to_remove.is_empty()
        || !apns_tokens_to_remove.is_empty()
    {
        subscription
            .web_push_subscriptions
            .retain(|sub| !endpoints_to_remove.contains(&sub.endpoint));
        subscription
            .fcm_tokens
            .retain(|token| !fcm_tokens_to_remove.contains(token));
        subscription
            .apns_tokens
            .retain(|token| !apns_tokens_to_remove.contains(token));

        if !subscription.is_empty() {
            db_handler.save_subscription(
                &subscription.subscriber,
                subscription_id,
                &subscription,
            )?;
        }
    }

    if subscription.is_empty() {
        db_handler.delete_subscription(&subscription.subscriber, subscription_id)?;
    }

    Ok(())
}

#[derive(Default)]
struct NotificationTargetRemoval {
    web_push_endpoint: Option<String>,
    fcm_token: Option<String>,
    apns_token: Option<String>,
}

fn describe_error_chain(error: &dyn Error) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(next) = source {
        parts.push(next.to_string());
        source = next.source();
    }
    parts.join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::{EventBuilder, Keys, Tag};

    #[test]
    fn large_encrypted_message_payload_keeps_mobile_decrypt_fields() {
        let keys = Keys::generate();
        let mut tags = Vec::new();
        for index in 0..80 {
            let value = format!("{}-{index}", "x".repeat(100));
            tags.push(Tag::parse(&["noise", value.as_str()]).expect("noise tag"));
        }
        tags.push(Tag::parse(&["header", "recipient", "ciphertext"]).expect("header tag"));
        let event = EventBuilder::new(Kind::from(1060), "encrypted rumor", tags)
            .to_event(&keys)
            .expect("event");
        assert!(
            serde_json::to_vec(&event).expect("event json").len() > 4096,
            "fixture must trigger compact event details"
        );

        let payload = create_event_payload(&event);

        let EventPayload::Details(details) = payload else {
            panic!("large event should use details payload");
        };
        assert_eq!(details.id, event.id.to_hex());
        assert_eq!(details.author, event.pubkey.to_hex());
        assert_eq!(details.kind, 1060);
        assert_eq!(details.created_at, Some(event.created_at.as_u64()));
        assert_eq!(details.content.as_deref(), Some("encrypted rumor"));
        let sig = event.sig.to_string();
        assert_eq!(details.sig.as_deref(), Some(sig.as_str()));
        assert_eq!(
            details.tags,
            Some(vec![vec![
                "header".to_string(),
                "recipient".to_string(),
                "ciphertext".to_string(),
            ]])
        );
    }
}
