use reqwest::Client;
use std::fs;
use std::sync::Arc;
use tokio::sync::Mutex;
use nostr_sdk::nostr::{Kind, Tag, Timestamp};
use nostr_sdk::UnsignedEvent;
use tokio::time::sleep;
use std::time::Duration;
use log::debug;

#[path = "common.rs"]
mod common;
use common::*;

async fn test_info_endpoint(client: &Client) {
    let response = client
        .get("http://0.0.0.0:3030/info")
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    
    let json_response = response.json::<serde_json::Value>().await
        .expect("Failed to parse JSON response");
    
    assert!(json_response.get("vapid_public_key").is_some());
}

async fn test_subscription_endpoints(client: &Client, push_port: u16, webhook_port: u16) {
    let (subscriber_keys, _) = get_test_keys_pair();
    let pubkey = subscriber_keys.public_key().to_string();
    let base_url = format!("http://0.0.0.0:3030/subscriptions/{}", pubkey);

    // Generate browser keys for web push
    let browser_keys = generate_browser_keys();
    let push_subscription = create_push_subscription(
        &browser_keys,
        &format!("http://0.0.0.0:{}/push", push_port)
    );

    // Test POST /subscriptions
    let new_subscription = serde_json::json!({
        "webhooks": [format!("http://0.0.0.0:{}/webhook", webhook_port)],
        "web_push_subscriptions": [push_subscription],
        "filter": {
            "#p": [pubkey]
        }
    });

    let response = make_authed_request(
        client,
        reqwest::Method::POST,
        &base_url,
        Some(new_subscription)
    ).await;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
}

async fn test_event_endpoint(
    client: &Client, 
    received_pushes: Arc<Mutex<Vec<serde_json::Value>>>,
    received_webhooks: Arc<Mutex<Vec<serde_json::Value>>>
) {
    // Clear existing notifications
    received_pushes.lock().await.clear();
    received_webhooks.lock().await.clear();
    
    let (subscriber_keys, sender_keys) = get_test_keys_pair();

    // Create and sign event from sender to subscriber
    let unsigned_event = UnsignedEvent::new(
        sender_keys.public_key(),
        Timestamp::now(),
        Kind::from(1),
        vec![Tag::public_key(subscriber_keys.public_key())],
        "Test content".to_string(),
    );

    let event = unsigned_event.sign(&sender_keys)
        .expect("Failed to sign event");

    println!("Sending test event to server...");
    debug!("Event being sent: {:?}", event);
    let response = client
        .post("http://0.0.0.0:3030/events")
        .json(&event)
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), reqwest::StatusCode::OK);

    // Add a small delay to ensure webhooks are processed
    sleep(Duration::from_millis(100)).await;

    println!("Checking webhook results...");
    let webhook_count = received_webhooks.lock().await.len();
    println!("Current webhook count: {}", webhook_count);

    // Verify both push notification and webhook were received
    let pushes = received_pushes.lock().await;
    assert_eq!(pushes.len(), 1);
    println!("✅ Push notification received");

    let webhooks = received_webhooks.lock().await;
    assert_eq!(webhooks.len(), 1);
    println!("✅ Webhook notification received");

    // Verify webhook payload format only
    let webhook_payload = &webhooks[0];
    assert!(webhook_payload.get("event").is_some(), "Payload should contain event");
    assert!(webhook_payload.get("title").is_some(), "Payload should contain title");
    assert!(webhook_payload.get("body").is_some(), "Payload should contain body");
    assert!(webhook_payload.get("icon").is_some(), "Payload should contain icon");
    assert!(webhook_payload.get("url").is_some(), "Payload should contain url");
    
    // Verify the content matches
    assert_eq!(webhook_payload["title"].as_str().unwrap(), "New Nostr Mention");
    assert_eq!(webhook_payload["body"].as_str().unwrap(), "Test content");
    assert!(webhook_payload["url"].as_str().unwrap().contains("https://iris.to/note1"));
}

#[tokio::test]
async fn test_integration() {
    let mut child = start_server().await;
    let client = Client::new();
    let (push_port, received_pushes) = start_mock_push_server().await;
    let (webhook_port, received_webhooks) = start_mock_webhook_server().await;

    test_info_endpoint(&client).await;
    test_subscription_endpoints(&client, push_port, webhook_port).await;
    
    test_event_endpoint(&client, received_pushes, received_webhooks).await;

    // Send SIGTERM to the child process
    child.kill().await.expect("Failed to send signal to process");
    child.wait().await.expect("Failed to wait for process to exit");
    
    // Clean up test directory after test
    let _ = fs::remove_dir_all("test_db");
}
