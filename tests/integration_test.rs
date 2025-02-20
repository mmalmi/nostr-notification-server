use std::fs;
use std::sync::Arc;
use tokio::sync::Mutex;
use nostr_sdk::nostr::{Kind, Tag, Timestamp};
use nostr_sdk::UnsignedEvent;
use tokio::time::sleep;
use std::time::Duration;
use log::debug;
use reqwest::{Client, Method, StatusCode};

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

async fn test_author_subscription_endpoints(client: &Client, push_port: u16, webhook_port: u16) {
    let (_subscriber_keys, author_keys) = get_test_keys_pair();
    let author_pubkey = author_keys.public_key().to_string();

    // Generate browser keys for web push
    let browser_keys = generate_browser_keys();
    let push_subscription = create_push_subscription(
        &browser_keys,
        &format!("http://0.0.0.0:{}/push", push_port)
    );

    // Test POST /subscriptions with author filter
    let new_subscription = serde_json::json!({
        "webhooks": [format!("http://0.0.0.0:{}/webhook", webhook_port)],
        "web_push_subscriptions": [push_subscription],
        "filter": {
            "authors": [author_pubkey],
            "kinds": [4]
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

    // Verify subscription was created correctly
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
    
    // Verify the filter contains correct author and kind
    assert_eq!(subscription["filter"]["authors"][0].as_str().unwrap(), author_pubkey);
    assert_eq!(subscription["filter"]["kinds"][0].as_i64().unwrap(), 4);
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

async fn test_failed_push_endpoint_removal(
    client: &Client,
    push_port: u16,
    webhook_port: u16
) {
    let (subscriber_keys, author_keys) = get_test_keys_pair();
    let pubkey = subscriber_keys.public_key().to_string();

    // Generate browser keys for web push
    let browser_keys = generate_browser_keys();
    
    // Create two push subscriptions - one valid, one with invalid endpoint
    let valid_push = create_push_subscription(
        &browser_keys,
        &format!("http://0.0.0.0:{}/push", push_port)
    );
    
    let invalid_push = create_push_subscription(
        &browser_keys,
        &format!("http://0.0.0.0:{}/non-existent", push_port) // This will return 404
    );

    // Create subscription with both endpoints
    let new_subscription = serde_json::json!({
        "webhooks": [format!("http://0.0.0.0:{}/webhook", webhook_port)],
        "web_push_subscriptions": [valid_push.clone(), invalid_push.clone()],
        "filter": {
            "#p": [pubkey.clone()]
        }
    });

    // Create subscription
    let response = make_authed_request(
        client,
        reqwest::Method::POST,
        "http://0.0.0.0:3030/subscriptions",
        Some(new_subscription)
    ).await;
    
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let subscription_id = response.json::<serde_json::Value>().await
        .unwrap()["id"].as_str().unwrap().to_string();

    // Create and send test event
    let event = create_test_event(&author_keys, &pubkey);
    
    client.post("http://0.0.0.0:3030/events")
        .json(&event)
        .send()
        .await
        .expect("Failed to send event");

    // Wait a bit for processing
    sleep(Duration::from_millis(100)).await;

    // Verify subscription was updated
    let response = make_authed_request(
        client,
        reqwest::Method::GET,
        &format!("http://0.0.0.0:3030/subscriptions/{}", subscription_id),
        None
    ).await;
    
    let subscription = response.json::<serde_json::Value>().await.unwrap();

    println!("Subscription after event: {:?}", subscription);
    let push_subs = subscription["web_push_subscriptions"].as_array().unwrap();
    
    assert_eq!(push_subs.len(), 1, "Should have removed invalid subscription");
    assert_eq!(
        push_subs[0]["endpoint"].as_str().unwrap(),
        valid_push.endpoint,
        "Should keep valid subscription"
    );
}

#[tokio::test]
async fn test_integration() {
    let mut child = start_server().await;
    let client = Client::new();
    let (push_port, received_pushes) = start_mock_push_server().await;
    let (webhook_port, received_webhooks) = start_mock_webhook_server().await;

    test_info_endpoint(&client).await;
    test_subscription_endpoints(&client, push_port, webhook_port).await;
    test_author_subscription_endpoints(&client, push_port, webhook_port).await;
    test_event_endpoint(&client, received_pushes, received_webhooks).await;
    test_failed_push_endpoint_removal(&client, push_port, webhook_port).await;
    test_subscription_cors(&client, push_port, webhook_port).await;

    // Send SIGTERM to the child process
    child.kill().await.expect("Failed to send signal to process");
    child.wait().await.expect("Failed to wait for process to exit");
    
    // Clean up test directory after test
    let _ = fs::remove_dir_all("test_db");
}

// Move the test function here but make it a regular async function
async fn test_subscription_cors(client: &Client, push_port: u16, webhook_port: u16) {
    let (subscriber_keys, _) = get_test_keys_pair();
    let pubkey = subscriber_keys.public_key().to_string();

    // Generate browser keys for web push
    let browser_keys = generate_browser_keys();
    let push_subscription = create_push_subscription(
        &browser_keys,
        &format!("http://0.0.0.0:{}/push", push_port)
    );

    let new_subscription = serde_json::json!({
        "webhooks": [format!("http://0.0.0.0:{}/webhook", webhook_port)],
        "web_push_subscriptions": [push_subscription],
        "filter": {
            "#p": [pubkey]
        }
    });

    // 1. Test POST new subscription with CORS
    let response = make_authed_request_with_keys(
        &client,
        Method::POST,
        "http://0.0.0.0:3030/subscriptions",
        Some(new_subscription),
        &subscriber_keys,
    )
    .await;

    assert_eq!(response.status(), StatusCode::CREATED);

    let created = response.json::<serde_json::Value>().await.unwrap();
    let subscription_id = created["id"].as_str().unwrap();

    // 2. Test GET subscription with CORS
    let get_url = format!("http://0.0.0.0:3030/subscriptions/{}", subscription_id);
    let response = make_authed_request_with_keys(
        &client,
        Method::GET,
        &get_url,
        None,
        &subscriber_keys,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);

    // 3. Test OPTIONS preflight for subscription update
    let response = client
        .request(Method::OPTIONS, &get_url)
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "authorization, content-type")
        .header("Origin", "*")
        .send()
        .await
        .expect("Failed to send preflight OPTIONS request");

    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers();
    assert_eq!(
        headers.get("access-control-allow-origin").unwrap().to_str().unwrap(),
        "*"
    );
    assert!(
        headers.get("access-control-allow-methods").unwrap().to_str().unwrap()
            .split(",").any(|m| m.trim() == "POST"),
        "Should allow POST method"
    );
    assert!(
        headers.get("access-control-allow-headers").unwrap().to_str().unwrap()
            .split(",").any(|h| h.trim().eq_ignore_ascii_case("authorization")),
        "Should allow Authorization header"
    );
}
