use reqwest::Client;
use std::time::{Duration, Instant};
use std::fs;
use nostr_sdk::nostr::{Kind, Tag, Timestamp, SecretKey};
use nostr_sdk::{Keys, UnsignedEvent};
use rand::Rng;
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

const SUBSCRIPTION_COUNT: usize = 100;

async fn create_test_subscriptions(
    client: &Client,
    count: usize,
    target_pubkey: &str,
    push_port: u16,
    webhook_port: u16,
) -> Vec<String> {
    let start_time = Instant::now();
    let mut pubkeys = Vec::with_capacity(count);
    let mut rng = rand::thread_rng();
    let mut last_batch_time = start_time;
    let batch_size = 100;

    for i in 0..count {
        let subscription_start = Instant::now();
        
        // Time key generation
        let key_start = Instant::now();
        let mut secret_key_bytes = [0u8; 32];
        rng.fill(&mut secret_key_bytes);
        let secret_key = SecretKey::from_slice(&secret_key_bytes)
            .expect("Failed to create secret key");
        let keys = Keys::new(secret_key);
        let pubkey = keys.public_key().to_string();
        let key_time = key_start.elapsed();
        pubkeys.push(pubkey.clone());

        // Time subscription creation
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

        // Time the POST request
        let request_start = Instant::now();
        let response = make_authed_request_with_keys(
            client,
            reqwest::Method::POST,
            "http://0.0.0.0:3030/subscriptions",
            Some(subscription),
            &keys
        ).await;
        let post_time = request_start.elapsed();
        assert_eq!(response.status(), reqwest::StatusCode::CREATED);

        // Time the GET request with same key
        let list_start = Instant::now();
        let list_response = make_authed_request_with_keys(
            client,
            reqwest::Method::GET,
            "http://0.0.0.0:3030/subscriptions",
            None,
            &keys
        ).await;
        let list_time = list_start.elapsed();
        assert_eq!(list_response.status(), reqwest::StatusCode::OK);

        if (i + 1) % batch_size == 0 {
            let batch_duration = last_batch_time.elapsed();
            println!("Batch {} - {} stats:", i + 1 - batch_size, i + 1);
            println!("  Total time: {:?}", batch_duration);
            println!("  Avg per subscription: {:?}", batch_duration / batch_size as u32);
            println!("  Last subscription breakdown:");
            println!("    - Key generation: {:?}", key_time);
            println!("    - POST time: {:?}", post_time);
            println!("    - GET time: {:?}", list_time);
            println!("    - Total: {:?}", subscription_start.elapsed());
            last_batch_time = Instant::now();
        }
    }

    let total_time = start_time.elapsed();
    println!("\nSubscription creation summary:");
    println!("Total time: {:?}", total_time);
    println!("Average time per subscription: {:?}", total_time / count as u32);

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

    println!("\n=== Setup Phase ===");
    println!("Creating {} subscriptions...", SUBSCRIPTION_COUNT);
    let setup_start = Instant::now();
    let _pubkeys = create_test_subscriptions(
        &client,
        SUBSCRIPTION_COUNT,
        &target_pubkey,
        push_port,
        webhook_port
    ).await;
    println!("Setup completed in: {:?}", setup_start.elapsed());

    // Clear any notifications from setup
    received_pushes.lock().await.clear();
    received_webhooks.lock().await.clear();

    println!("\n=== Event Processing Phase ===");
    // Create and send test event
    let event_creation_start = Instant::now();
    let unsigned_event = UnsignedEvent::new(
        sender_keys.public_key(),
        Timestamp::now(),
        Kind::from(1),
        vec![Tag::public_key(subscriber_keys.public_key())],
        "Performance test content".to_string(),
    );
    let event = unsigned_event.sign(&sender_keys)
        .expect("Failed to sign event");
    println!("Event creation time: {:?}", event_creation_start.elapsed());

    // Time the event sending
    println!("Sending test event...");
    let send_start = Instant::now();
    let response = client
        .post("http://0.0.0.0:3030/events")
        .json(&event)
        .send()
        .await
        .expect("Failed to send request");
    println!("Event send time: {:?}", send_start.elapsed());
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    // Time the webhook processing
    let processing_start = Instant::now();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let webhooks_clone = Arc::clone(&received_webhooks);

    tokio::spawn(async move {
        let mut attempts = 0;
        loop {
            let webhooks = webhooks_clone.lock().await;
            if webhooks.len() > 0 {
                println!("Webhook received after {} polling attempts", attempts);
                let _ = tx.send(());
                break;
            }
            drop(webhooks);
            attempts += 1;
            tokio::time::sleep(Duration::from_micros(100)).await;
        }
    });

    match tokio::time::timeout(Duration::from_secs(5), rx).await {
        Ok(_) => {
            let total_processing = processing_start.elapsed();
            println!("\n=== Performance Summary ===");
            println!("Event processing breakdown:");
            println!("  Creation time: {:?}", event_creation_start.elapsed());
            println!("  Send time: {:?}", send_start.elapsed());
            println!("  Processing time until webhook: {:?}", total_processing);
            println!("Total end-to-end time: {:?}", event_creation_start.elapsed());
        },
        Err(_) => println!("❌ Timeout waiting for webhook processing!"),
    }

    // Cleanup
    child.kill().await.expect("Failed to kill server");
    child.wait().await.expect("Failed to wait for server exit");
    let _ = fs::remove_dir_all("test_db");
} 