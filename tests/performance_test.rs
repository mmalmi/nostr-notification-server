use reqwest::Client;
use std::time::{Duration, Instant};
use std::fs;
use nostr_sdk::nostr::{Kind, Tag, Timestamp, SecretKey};
use nostr_sdk::{Keys, UnsignedEvent};
use rand::Rng;
use std::io::Write;
use std::sync::Arc;

#[path = "common.rs"]
mod common;

use common::{
    start_server,
    get_test_keys_pair,
    make_authed_request_with_keys,
    start_mock_push_server,
    start_mock_webhook_server,
};

const SUBSCRIPTION_COUNT: usize = 1000;

async fn create_test_subscriptions(
    client: &Client,
    count: usize,
    target_pubkey: &str,
    push_port: u16,
    webhook_port: u16,
) -> Vec<String> {
    let mut pubkeys = Vec::with_capacity(count);
    let mut rng = rand::thread_rng();

    for i in 0..count {
        let mut secret_key_bytes = [0u8; 32];
        rng.fill(&mut secret_key_bytes);
        let secret_key = SecretKey::from_slice(&secret_key_bytes)
            .expect("Failed to create secret key");
        let keys = Keys::new(secret_key);
        let pubkey = keys.public_key().to_string();
        pubkeys.push(pubkey.clone());

        // Last subscription watches for events tagging target_pubkey
        let filter = if i == count - 1 {
            serde_json::json!({
                "#p": [target_pubkey]
            })
        } else {
            serde_json::json!({
                "#p": [format!("non_matching_pubkey_{}", i)]
            })
        };

        let subscription = serde_json::json!({
            "webhooks": [format!("http://0.0.0.0:{}/webhook", webhook_port)],
            "web_push_subscriptions": [{
                "endpoint": format!("http://0.0.0.0:{}/push", push_port),
                "auth": "b0h1T9+PxguADzE3J2t3Qw==",
                "p256dh": "BBvt0/6jK6BfNwEOtek0t7OA/5GNuScH8/5SVeZ9RCCcQ/SmUfSGvde8OMBHTeOQlHbXlsECZ+ew1mISL6WsvDY="
            }],
            "filter": filter
        });

        let response = make_authed_request_with_keys(
            client,
            reqwest::Method::POST,
            "http://0.0.0.0:3030/subscriptions",
            Some(subscription),
            &keys
        ).await;
        assert_eq!(response.status(), reqwest::StatusCode::CREATED);

        if i > 0 && i % 1000 == 0 {
            print!(".");
            std::io::stdout().flush().unwrap();
        }
    }
    println!();

    pubkeys
}

#[tokio::test]
async fn test_subscription_matching_performance() {
    let mut child = start_server().await;
    let client = Client::new();
    let (push_port, received_pushes) = start_mock_push_server().await;
    let (webhook_port, received_webhooks) = start_mock_webhook_server().await;

    // Create target subscription that should match our event
    let (subscriber_keys, sender_keys) = get_test_keys_pair();
    let target_pubkey = subscriber_keys.public_key().to_string();

    println!("Creating {} subscriptions...", SUBSCRIPTION_COUNT);
    let start_time = Instant::now();
    let _pubkeys = create_test_subscriptions(
        &client,
        SUBSCRIPTION_COUNT,
        &target_pubkey,
        push_port,
        webhook_port
    ).await;
    let setup_duration = start_time.elapsed();
    println!("Setup time: {:?}", setup_duration);

    // Clear any notifications from setup
    received_pushes.lock().await.clear();
    received_webhooks.lock().await.clear();

    // Create and send test event
    println!("Sending test event...");
    let start_time = Instant::now();
    let unsigned_event = UnsignedEvent::new(
        sender_keys.public_key(),
        Timestamp::now(),
        Kind::from(1),
        vec![Tag::public_key(subscriber_keys.public_key())],
        "Performance test content".to_string(),
    );

    let event = unsigned_event.sign(&sender_keys)
        .expect("Failed to sign event");

    let response = client
        .post("http://0.0.0.0:3030/events")
        .json(&event)
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), reqwest::StatusCode::OK);

    // Wait for webhook to be received instead of arbitrary sleep
    let (tx, rx) = tokio::sync::oneshot::channel();
    let webhooks_clone = Arc::clone(&received_webhooks);

    tokio::spawn(async move {
        loop {
            let webhooks = webhooks_clone.lock().await;
            if webhooks.len() > 0 {
                let _ = tx.send(());
                break;
            }
            drop(webhooks);
            tokio::time::sleep(Duration::from_micros(100)).await;
        }
    });

    tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("Webhook timeout")
        .expect("Channel closed");

    let processing_duration = start_time.elapsed();
    println!("⏱️ Total time from event send to webhook received: {:?}", processing_duration);

    // Cleanup
    child.kill().await.expect("Failed to kill server");
    child.wait().await.expect("Failed to wait for server exit");
    let _ = fs::remove_dir_all("test_db");
} 