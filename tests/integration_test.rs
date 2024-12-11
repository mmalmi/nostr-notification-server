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

    // Create subscription and get the ID from response
    let response = make_authed_request(
        client,
        reqwest::Method::POST,
        "http://0.0.0.0:3030/subscriptions",
        Some(new_subscription.clone())
    ).await;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    
    let created_response = response.json::<serde_json::Value>().await
        .expect("Failed to parse JSON response");
    let subscription_id = created_response["id"].as_str()
        .expect("Response should contain subscription ID");

    // Check that the subscription ID has a length greater than zero
    assert!(!subscription_id.is_empty(), "Subscription ID should not be empty");

    // Test GET /subscriptions/:id using the received ID
    let get_url = format!("http://0.0.0.0:3030/subscriptions/{}", subscription_id);
    let response = make_authed_request(
        client,
        reqwest::Method::GET,
        &get_url,
        None
    ).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    
    let subscription = response.json::<serde_json::Value>().await
        .expect("Failed to parse JSON response");
    assert_eq!(subscription["webhooks"][0].as_str().unwrap(), 
               format!("http://0.0.0.0:{}/webhook", webhook_port));

    // Test GET /subscriptions (list all)
    let response = make_authed_request(
        client,
        reqwest::Method::GET,
        "http://0.0.0.0:3030/subscriptions",
        None
    ).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    
    let subscriptions = response.json::<serde_json::Map<String, serde_json::Value>>().await
        .expect("Failed to parse JSON response");
    assert_eq!(subscriptions.len(), 1);
    assert!(subscriptions.contains_key(subscription_id));

    // Test updating existing subscription
    let updated_subscription = serde_json::json!({
        "webhooks": [format!("http://0.0.0.0:{}/webhook", webhook_port)],
        "web_push_subscriptions": [push_subscription],
        "filter": {
            "#p": [pubkey],
            "kinds": [1],
        }
    });

    println!("updated_subscription: {:?}", updated_subscription);
    println!("url: {}", get_url);

    let response = make_authed_request(
        client,
        reqwest::Method::POST,
        &get_url,
        Some(updated_subscription)
    ).await;

    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let response = client
        .get("http://0.0.0.0:3030")
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let json_response = response.json::<serde_json::Value>().await
        .expect("Failed to parse JSON response");

    // subscription count should be greater than 0
    assert!(json_response.get("subscriptions").is_some());
    assert!(json_response["subscriptions"].as_u64().unwrap() > 0);
    
    assert_eq!(json_response["version"].as_str().unwrap(), env!("CARGO_PKG_VERSION"));
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

    sleep(Duration::from_millis(100)).await;

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
    assert_eq!(webhook_payload["title"].as_str().unwrap(), "New Mention from Unknown");
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
