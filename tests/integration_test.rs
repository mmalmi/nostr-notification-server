use log::debug;
use nostr_sdk::nostr::{Kind, Tag, TagKind, Timestamp};
use nostr_sdk::UnsignedEvent;
use reqwest::{Client, Method, StatusCode};
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;

#[path = "common.rs"]
mod common;
use common::*;

async fn test_info_endpoint(client: &Client) {
    let response = client
        .get("http://127.0.0.1:3030/info")
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let json_response = response
        .json::<serde_json::Value>()
        .await
        .expect("Failed to parse JSON response");

    assert!(json_response.get("vapid_public_key").is_some());
}

async fn test_subscription_endpoints(client: &Client, push_port: u16, webhook_port: u16) {
    let (subscriber_keys, _) = get_test_keys_pair(0);
    let pubkey = subscriber_keys.public_key().to_string();

    // Generate browser keys for web push
    let browser_keys = generate_browser_keys();
    let push_subscription = create_push_subscription(
        &browser_keys,
        &format!("http://127.0.0.1:{}/push", push_port),
    );

    // Test POST /subscriptions
    let new_subscription = serde_json::json!({
        "webhooks": [format!("http://127.0.0.1:{}/webhook", webhook_port)],
        "web_push_subscriptions": [push_subscription],
        "filter": {
            "#p": [pubkey]
        }
    });

    // Create subscription and get the ID from response
    let response = make_authed_request(
        client,
        reqwest::Method::POST,
        "http://127.0.0.1:3030/subscriptions",
        Some(new_subscription.clone()),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);

    let created_response = response
        .json::<serde_json::Value>()
        .await
        .expect("Failed to parse JSON response");
    let subscription_id = created_response["id"]
        .as_str()
        .expect("Response should contain subscription ID");

    // Check that the subscription ID has a length greater than zero
    assert!(
        !subscription_id.is_empty(),
        "Subscription ID should not be empty"
    );

    // Test GET /subscriptions/:id using the received ID
    let get_url = format!("http://127.0.0.1:3030/subscriptions/{}", subscription_id);
    let response = make_authed_request(client, reqwest::Method::GET, &get_url, None).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let subscription = response
        .json::<serde_json::Value>()
        .await
        .expect("Failed to parse JSON response");
    assert_eq!(
        subscription["webhooks"][0].as_str().unwrap(),
        format!("http://127.0.0.1:{}/webhook", webhook_port)
    );

    // Test GET /subscriptions (list all)
    let response = make_authed_request(
        client,
        reqwest::Method::GET,
        "http://127.0.0.1:3030/subscriptions",
        None,
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let subscriptions = response
        .json::<serde_json::Map<String, serde_json::Value>>()
        .await
        .expect("Failed to parse JSON response");
    assert_eq!(subscriptions.len(), 1);
    assert!(subscriptions.contains_key(subscription_id));

    // Test updating existing subscription
    let updated_subscription = serde_json::json!({
        "webhooks": [format!("http://127.0.0.1:{}/webhook", webhook_port)],
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
        Some(updated_subscription),
    )
    .await;

    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let response = client
        .get("http://127.0.0.1:3030")
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let json_response = response
        .json::<serde_json::Value>()
        .await
        .expect("Failed to parse JSON response");

    // subscription count should be greater than 0
    assert!(json_response.get("subscriptions").is_some());
    assert!(json_response["subscriptions"].as_u64().unwrap() > 0);

    assert_eq!(
        json_response["version"].as_str().unwrap(),
        env!("CARGO_PKG_VERSION")
    );
}

async fn test_author_subscription_endpoints(client: &Client, push_port: u16, webhook_port: u16) {
    let (_subscriber_keys, author_keys) = get_test_keys_pair(1);
    let author_pubkey = author_keys.public_key().to_string();

    // Generate browser keys for web push
    let browser_keys = generate_browser_keys();
    let push_subscription = create_push_subscription(
        &browser_keys,
        &format!("http://127.0.0.1:{}/push", push_port),
    );

    // Test POST /subscriptions with author filter
    let new_subscription = serde_json::json!({
        "webhooks": [format!("http://127.0.0.1:{}/webhook", webhook_port)],
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
        "http://127.0.0.1:3030/subscriptions",
        Some(new_subscription.clone()),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);

    let created_response = response
        .json::<serde_json::Value>()
        .await
        .expect("Failed to parse JSON response");
    let subscription_id = created_response["id"]
        .as_str()
        .expect("Response should contain subscription ID");

    // Verify subscription was created correctly
    let get_url = format!("http://127.0.0.1:3030/subscriptions/{}", subscription_id);
    let response = make_authed_request(client, reqwest::Method::GET, &get_url, None).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let subscription = response
        .json::<serde_json::Value>()
        .await
        .expect("Failed to parse JSON response");

    // Verify the filter contains correct author and kind
    assert_eq!(
        subscription["filter"]["authors"][0].as_str().unwrap(),
        author_pubkey
    );
    assert_eq!(subscription["filter"]["kinds"][0].as_i64().unwrap(), 4);
}

async fn test_event_endpoint(
    client: &Client,
    push_port: u16,
    webhook_port: u16,
    received_pushes: Arc<Mutex<Vec<serde_json::Value>>>,
    received_webhooks: Arc<Mutex<Vec<serde_json::Value>>>,
) {
    // Clear existing notifications
    received_pushes.lock().await.clear();
    received_webhooks.lock().await.clear();

    let (subscriber_keys, sender_keys) = get_test_keys_pair(2);
    let pubkey = subscriber_keys.public_key().to_string();

    // Generate browser keys for web push
    let browser_keys = generate_browser_keys();
    let push_subscription = create_push_subscription(
        &browser_keys,
        &format!("http://127.0.0.1:{}/push", push_port),
    );

    // Create subscription before sending test event
    let new_subscription = serde_json::json!({
        "webhooks": [format!("http://127.0.0.1:{}/webhook", webhook_port)],
        "web_push_subscriptions": [push_subscription],
        "filter": {
            "#p": [pubkey]
        }
    });

    // Create subscription
    let response = make_authed_request(
        client,
        reqwest::Method::POST,
        "http://127.0.0.1:3030/subscriptions",
        Some(new_subscription),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);

    // Test kind 1 event
    let unsigned_event = UnsignedEvent::new(
        sender_keys.public_key(),
        Timestamp::now(),
        Kind::from(1),
        vec![Tag::public_key(subscriber_keys.public_key())],
        "Test content".to_string(),
    );

    let event = unsigned_event
        .sign(&sender_keys)
        .expect("Failed to sign event");

    println!("Sending test event to server...");
    debug!("Event being sent: {:?}", event);
    let response = client
        .post("http://127.0.0.1:3030/events")
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

    // Verify webhook payload format for kind 1 event
    let webhook_payload = &webhooks[0];
    assert!(
        webhook_payload.get("event").is_some(),
        "Payload should contain event"
    );

    // Verify event details are present
    let event_details = &webhook_payload["event"];
    assert!(event_details.get("id").is_some(), "Event should contain id");
    assert!(
        event_details.get("pubkey").is_some(),
        "Event should contain author"
    );
    assert!(
        event_details.get("kind").is_some(),
        "Event should contain kind"
    );
    assert_eq!(
        event_details["kind"].as_u64().unwrap(),
        1,
        "Event kind should be 1"
    );

    assert!(
        webhook_payload.get("title").is_some(),
        "Payload should contain title"
    );
    assert!(
        webhook_payload.get("body").is_some(),
        "Payload should contain body"
    );
    assert!(
        webhook_payload.get("icon").is_some(),
        "Payload should contain icon"
    );
    assert!(
        webhook_payload.get("url").is_some(),
        "Payload should contain url"
    );

    assert_eq!(
        webhook_payload["title"].as_str().unwrap(),
        "Mention by Someone"
    );
    assert_eq!(webhook_payload["body"].as_str().unwrap(), "Test content");
    assert!(webhook_payload["url"]
        .as_str()
        .unwrap()
        .contains("https://iris.to/note1"));
}

async fn test_failed_push_endpoint_removal(client: &Client, push_port: u16, webhook_port: u16) {
    let (subscriber_keys, author_keys) = get_test_keys_pair(3);
    let pubkey = subscriber_keys.public_key().to_string();

    // Generate browser keys for web push
    let browser_keys = generate_browser_keys();

    // Create two push subscriptions - one valid, one with invalid endpoint
    let valid_push = create_push_subscription(
        &browser_keys,
        &format!("http://127.0.0.1:{}/push", push_port),
    );

    let invalid_push = create_push_subscription(
        &browser_keys,
        &format!("http://127.0.0.1:{}/non-existent", push_port), // This will return 404
    );

    // Create subscription with both endpoints
    let new_subscription = serde_json::json!({
        "webhooks": [format!("http://127.0.0.1:{}/webhook", webhook_port)],
        "web_push_subscriptions": [valid_push.clone(), invalid_push.clone()],
        "filter": {
            "#p": [pubkey.clone()]
        }
    });

    // Create subscription
    let response = make_authed_request(
        client,
        reqwest::Method::POST,
        "http://127.0.0.1:3030/subscriptions",
        Some(new_subscription),
    )
    .await;

    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let subscription_id = response.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Create and send test event
    let event = create_test_event(&author_keys, &pubkey);

    client
        .post("http://127.0.0.1:3030/events")
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
        &format!("http://127.0.0.1:3030/subscriptions/{}", subscription_id),
        None,
    )
    .await;

    let subscription = response.json::<serde_json::Value>().await.unwrap();

    println!("Subscription after event: {:?}", subscription);
    let push_subs = subscription["web_push_subscriptions"].as_array().unwrap();

    assert_eq!(
        push_subs.len(),
        1,
        "Should have removed invalid subscription"
    );
    assert_eq!(
        push_subs[0]["endpoint"].as_str().unwrap(),
        valid_push.endpoint,
        "Should keep valid subscription"
    );
}

async fn test_auth_failures(client: &Client, _push_port: u16, webhook_port: u16) {
    let (subscriber_keys, _) = get_test_keys_pair(4);
    let pubkey = subscriber_keys.public_key().to_string();

    // Create a test subscription payload
    let new_subscription = serde_json::json!({
        "webhooks": [format!("http://127.0.0.1:{}/webhook", webhook_port)],
        "web_push_subscriptions": [],
        "filter": {
            "#p": [pubkey]
        }
    });

    // Test with missing authorization header
    let response = client
        .post("http://127.0.0.1:3030/subscriptions")
        .json(&new_subscription)
        .send()
        .await
        .expect("Failed to send request");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Test with invalid authorization header format
    let response = client
        .post("http://127.0.0.1:3030/subscriptions/123")
        .header("Authorization", "InvalidFormat")
        .json(&new_subscription)
        .send()
        .await
        .expect("Failed to send request");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Test with invalid signature
    let response = client
        .post("http://127.0.0.1:3030/subscriptions")
        .header("Authorization", "Nostr AValidButIncorrectSignatureHere")
        .json(&new_subscription)
        .send()
        .await
        .expect("Failed to send request");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

async fn test_encrypted_dm_notifications(
    client: &Client,
    push_port: u16,
    webhook_port: u16,
    received_pushes: Arc<Mutex<Vec<serde_json::Value>>>,
    received_webhooks: Arc<Mutex<Vec<serde_json::Value>>>,
) {
    // Clear existing notifications at the start of the test
    received_pushes.lock().await.clear();
    received_webhooks.lock().await.clear();

    let (_subscriber_keys, sender_keys) = get_test_keys_pair(5);

    // Generate browser keys for web push
    let browser_keys = generate_browser_keys();
    let push_subscription = create_push_subscription(
        &browser_keys,
        &format!("http://127.0.0.1:{}/push", push_port),
    );

    // Create subscription for DMs
    let new_subscription = serde_json::json!({
        "webhooks": [format!("http://127.0.0.1:{}/webhook", webhook_port)],
        "web_push_subscriptions": [push_subscription],
        "filter": {
            "authors": [sender_keys.public_key().to_string()],
            "kinds": [4]  // Kind 4 for encrypted DMs
        }
    });

    // Create subscription
    let response = make_authed_request(
        client,
        Method::POST,
        "http://127.0.0.1:3030/subscriptions",
        Some(new_subscription),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    // Test kind 4 event (encrypted direct message)
    let unsigned_dm = UnsignedEvent::new(
        sender_keys.public_key(),
        Timestamp::now(),
        Kind::EncryptedDirectMessage,
        vec![Tag::custom(
            TagKind::custom("header"),
            vec!["encrypted stuff here"],
        )],
        "Encrypted content".to_string(),
    );

    let dm_event = unsigned_dm
        .sign(&sender_keys)
        .expect("Failed to sign DM event");

    println!("Sending test DM event to server...");
    let response = client
        .post("http://127.0.0.1:3030/events")
        .json(&dm_event)
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), reqwest::StatusCode::OK);

    sleep(Duration::from_millis(100)).await;

    // Verify notifications for DM
    let pushes = received_pushes.lock().await;
    assert_eq!(pushes.len(), 1, "Should receive push notification for DM");

    let webhooks = received_webhooks.lock().await;
    assert_eq!(
        webhooks.len(),
        1,
        "Should receive webhook notification for DM"
    );

    // Verify DM webhook payload format
    let dm_payload = &webhooks[0];
    assert_eq!(dm_payload["title"].as_str().unwrap(), "DM by Someone");
    assert_eq!(dm_payload["body"].as_str().unwrap(), ""); // DM content should be empty in notification
    assert!(dm_payload["url"]
        .as_str()
        .unwrap()
        .contains("https://iris.to/note1"));
}

async fn test_subscription_author_update(
    client: &Client,
    push_port: u16,
    webhook_port: u16,
    received_pushes: Arc<Mutex<Vec<serde_json::Value>>>,
    received_webhooks: Arc<Mutex<Vec<serde_json::Value>>>,
) {
    // Clear existing notifications
    received_pushes.lock().await.clear();
    received_webhooks.lock().await.clear();

    let (subscriber_keys, author_keys) = get_test_keys_pair(6);
    let author_pubkey = author_keys.public_key().to_string();
    let subscriber_pubkey = subscriber_keys.public_key().to_string();

    // Generate browser keys for web push
    let browser_keys = generate_browser_keys();
    let push_subscription = create_push_subscription(
        &browser_keys,
        &format!("http://127.0.0.1:{}/push", push_port),
    );

    // Create initial subscription without author filter
    let initial_subscription = serde_json::json!({
        "webhooks": [format!("http://127.0.0.1:{}/webhook", webhook_port)],
        "web_push_subscriptions": [push_subscription],
        "filter": {
            "kinds": [1]
        }
    });

    // Create subscription and get the ID
    let response = make_authed_request(
        client,
        Method::POST,
        "http://127.0.0.1:3030/subscriptions",
        Some(initial_subscription),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let subscription_id = response.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Update subscription to include author filter
    let updated_subscription = serde_json::json!({
        "webhooks": [format!("http://127.0.0.1:{}/webhook", webhook_port)],
        "web_push_subscriptions": [push_subscription],
        "filter": {
            "authors": [author_pubkey],
            "kinds": [1]
        }
    });

    // Update the subscription
    let response = make_authed_request(
        client,
        Method::POST,
        &format!("http://127.0.0.1:3030/subscriptions/{}", subscription_id),
        Some(updated_subscription),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    // Create and send test event from the author
    let event = create_test_event(&author_keys, &subscriber_pubkey);

    let response = client
        .post("http://127.0.0.1:3030/events")
        .json(&event)
        .send()
        .await
        .expect("Failed to send event");
    assert_eq!(response.status(), StatusCode::OK);

    // Wait for notifications
    sleep(Duration::from_millis(100)).await;

    // Verify notifications were received
    let pushes = received_pushes.lock().await;
    assert_eq!(
        pushes.len(),
        1,
        "Should receive push notification for author's event"
    );

    let webhooks = received_webhooks.lock().await;
    assert_eq!(
        webhooks.len(),
        1,
        "Should receive webhook notification for author's event"
    );
}

async fn test_mobile_subscription_endpoints(client: &Client) {
    let (subscriber_keys, _) = get_test_keys_pair(9);
    let pubkey = subscriber_keys.public_key().to_string();
    let fcm_token = "fcm-test-token-123";
    let apns_token = "apns-test-token-456";

    let new_subscription = serde_json::json!({
        "webhooks": [],
        "web_push_subscriptions": [],
        "fcm_tokens": [fcm_token],
        "apns_tokens": [apns_token],
        "filter": {
            "#p": [pubkey]
        }
    });

    let response = make_authed_request_with_keys(
        client,
        Method::POST,
        "http://127.0.0.1:3030/subscriptions",
        Some(new_subscription),
        &subscriber_keys,
    )
    .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    let created = response.json::<serde_json::Value>().await.unwrap();
    let subscription_id = created["id"].as_str().unwrap();

    let get_url = format!("http://127.0.0.1:3030/subscriptions/{}", subscription_id);
    let response =
        make_authed_request_with_keys(client, Method::GET, &get_url, None, &subscriber_keys).await;
    assert_eq!(response.status(), StatusCode::OK);

    let subscription = response.json::<serde_json::Value>().await.unwrap();
    assert_eq!(subscription["fcm_tokens"], serde_json::json!([fcm_token]));
    assert_eq!(subscription["apns_tokens"], serde_json::json!([apns_token]));

    let response = make_authed_request_with_keys(
        client,
        Method::GET,
        "http://127.0.0.1:3030/subscriptions",
        None,
        &subscriber_keys,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let subscriptions = response
        .json::<serde_json::Map<String, serde_json::Value>>()
        .await
        .unwrap();
    let listed = subscriptions
        .get(subscription_id)
        .expect("subscription missing from list");
    assert_eq!(listed["fcm_tokens"], serde_json::json!([fcm_token]));
    assert_eq!(listed["apns_tokens"], serde_json::json!([apns_token]));
}

async fn test_mobile_push_delivery(
    client: &Client,
    received_fcm: Arc<Mutex<Vec<serde_json::Value>>>,
    received_apns: Arc<Mutex<Vec<serde_json::Value>>>,
) {
    received_fcm.lock().await.clear();
    received_apns.lock().await.clear();

    let (subscriber_keys, sender_keys) = get_test_keys_pair(10);
    let pubkey = subscriber_keys.public_key().to_string();
    let fcm_token = "fcm-mobile-delivery-token";
    let apns_token = "apns-mobile-delivery-token";

    let new_subscription = serde_json::json!({
        "webhooks": [],
        "web_push_subscriptions": [],
        "fcm_tokens": [fcm_token],
        "apns_tokens": [apns_token],
        "filter": {
            "kinds": [1060],
            "#p": [pubkey.clone()]
        }
    });

    let response = make_authed_request(
        client,
        Method::POST,
        "http://127.0.0.1:3030/subscriptions",
        Some(new_subscription),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let event = UnsignedEvent::new(
        sender_keys.public_key(),
        Timestamp::now(),
        Kind::from(1060),
        vec![Tag::public_key(subscriber_keys.public_key())],
        "ciphertext".to_string(),
    )
    .sign(&sender_keys)
    .expect("Failed to sign mobile push test event");

    let response = client
        .post("http://127.0.0.1:3030/events")
        .json(&event)
        .send()
        .await
        .expect("Failed to send mobile push event");
    assert_eq!(response.status(), StatusCode::OK);

    sleep(Duration::from_millis(200)).await;

    let fcm_requests = received_fcm.lock().await;
    assert_eq!(fcm_requests.len(), 1, "Expected one FCM delivery attempt");
    let fcm = &fcm_requests[0];
    assert_eq!(fcm["project_id"].as_str().unwrap(), TEST_FCM_PROJECT_ID);
    assert_eq!(
        fcm["headers"]["authorization"].as_str().unwrap(),
        "Bearer test-fcm-access-token"
    );
    assert_eq!(fcm["body"]["message"]["token"].as_str().unwrap(), fcm_token);
    let fcm_event = serde_json::from_str::<serde_json::Value>(
        fcm["body"]["message"]["data"]["event"].as_str().unwrap(),
    )
    .expect("FCM event payload should be JSON");
    assert_eq!(fcm_event["id"].as_str().unwrap(), event.id.to_hex());
    assert_eq!(fcm_event["kind"].as_u64().unwrap(), 1060);

    let apns_requests = received_apns.lock().await;
    assert_eq!(apns_requests.len(), 1, "Expected one APNS delivery attempt");
    let apns = &apns_requests[0];
    assert_eq!(apns["token"].as_str().unwrap(), apns_token);
    assert_eq!(apns["headers"]["apns-topic"].as_str().unwrap(), "to.iris");
    assert_eq!(
        apns["headers"]["apns-push-type"].as_str().unwrap(),
        "background"
    );
    assert_eq!(
        apns["body"]["event"]["id"].as_str().unwrap(),
        event.id.to_hex()
    );
    assert_eq!(apns["body"]["event"]["kind"].as_u64().unwrap(), 1060);
    assert_eq!(
        apns["body"]["event"]["pubkey"].as_str().unwrap(),
        event.pubkey.to_hex()
    );
    assert_eq!(
        apns["body"]["event"]["content"].as_str().unwrap(),
        "ciphertext"
    );
    assert_eq!(
        apns["body"]["aps"]["content-available"].as_u64().unwrap(),
        1
    );
}

async fn test_mobile_push_token_moves_between_subscriptions(
    client: &Client,
    received_fcm: Arc<Mutex<Vec<serde_json::Value>>>,
) {
    received_fcm.lock().await.clear();

    let (old_subscriber_keys, old_author_keys) = get_test_keys_pair(11);
    let (new_subscriber_keys, new_author_keys) = get_test_keys_pair(12);
    let fcm_token = "fcm-token-moves-between-accounts:with-colon";

    let old_subscription = serde_json::json!({
        "webhooks": [],
        "web_push_subscriptions": [],
        "fcm_tokens": [fcm_token],
        "apns_tokens": [],
        "filter": {
            "kinds": [1060],
            "authors": [old_author_keys.public_key().to_hex()]
        }
    });
    let response = make_authed_request_with_keys(
        client,
        Method::POST,
        "http://127.0.0.1:3030/subscriptions",
        Some(old_subscription),
        &old_subscriber_keys,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let old_created = response.json::<serde_json::Value>().await.unwrap();
    let old_subscription_id = old_created["id"].as_str().unwrap().to_string();

    let new_subscription = serde_json::json!({
        "webhooks": [],
        "web_push_subscriptions": [],
        "fcm_tokens": [fcm_token],
        "apns_tokens": [],
        "filter": {
            "kinds": [1060],
            "authors": [new_author_keys.public_key().to_hex()]
        }
    });
    let response = make_authed_request_with_keys(
        client,
        Method::POST,
        "http://127.0.0.1:3030/subscriptions",
        Some(new_subscription),
        &new_subscriber_keys,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let old_get_url = format!(
        "http://127.0.0.1:3030/subscriptions/{}",
        old_subscription_id
    );
    let response = make_authed_request_with_keys(
        client,
        Method::GET,
        &old_get_url,
        None,
        &old_subscriber_keys,
    )
    .await;
    let old_get_status = response.status();
    let old_get_body = response.text().await.unwrap_or_default();
    assert_eq!(
        old_get_status,
        StatusCode::NOT_FOUND,
        "Moving an FCM token to a new account should remove the old empty subscription; body: {}",
        old_get_body
    );

    let old_event = UnsignedEvent::new(
        old_author_keys.public_key(),
        Timestamp::now(),
        Kind::from(1060),
        Vec::new(),
        "old ciphertext".to_string(),
    )
    .sign(&old_author_keys)
    .expect("Failed to sign old-author event");
    let response = client
        .post("http://127.0.0.1:3030/events")
        .json(&old_event)
        .send()
        .await
        .expect("Failed to send old-author event");
    assert_eq!(response.status(), StatusCode::OK);
    sleep(Duration::from_millis(200)).await;
    assert_eq!(
        received_fcm.lock().await.len(),
        0,
        "Old account subscription must not keep delivering to the moved FCM token"
    );

    let new_event = UnsignedEvent::new(
        new_author_keys.public_key(),
        Timestamp::now(),
        Kind::from(1060),
        Vec::new(),
        "new ciphertext".to_string(),
    )
    .sign(&new_author_keys)
    .expect("Failed to sign new-author event");
    let response = client
        .post("http://127.0.0.1:3030/events")
        .json(&new_event)
        .send()
        .await
        .expect("Failed to send new-author event");
    assert_eq!(response.status(), StatusCode::OK);
    sleep(Duration::from_millis(200)).await;

    let fcm_requests = received_fcm.lock().await;
    assert_eq!(fcm_requests.len(), 1, "Expected one FCM delivery attempt");
    assert_eq!(
        fcm_requests[0]["body"]["message"]["token"]
            .as_str()
            .unwrap(),
        fcm_token
    );
}

async fn test_seen_events_persistence(
    client: &Client,
    push_port: u16,
    webhook_port: u16,
    received_pushes: Arc<Mutex<Vec<serde_json::Value>>>,
    received_webhooks: Arc<Mutex<Vec<serde_json::Value>>>,
) {
    println!("Testing seen events persistence across server restarts...");

    // Clear existing notifications
    received_pushes.lock().await.clear();
    received_webhooks.lock().await.clear();

    let (subscriber_keys, sender_keys) = get_test_keys_pair(8);
    let pubkey = subscriber_keys.public_key().to_string();

    // Generate browser keys for web push
    let browser_keys = generate_browser_keys();
    let push_subscription = create_push_subscription(
        &browser_keys,
        &format!("http://127.0.0.1:{}/push", push_port),
    );

    // Create subscription
    let new_subscription = serde_json::json!({
        "webhooks": [format!("http://127.0.0.1:{}/webhook", webhook_port)],
        "web_push_subscriptions": [push_subscription],
        "filter": {
            "#p": [pubkey]
        }
    });

    let response = make_authed_request(
        client,
        Method::POST,
        "http://127.0.0.1:3030/subscriptions",
        Some(new_subscription),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    // Create and send first event
    let event = UnsignedEvent::new(
        sender_keys.public_key(),
        Timestamp::now(),
        Kind::from(1),
        vec![Tag::public_key(subscriber_keys.public_key())],
        "Test event for persistence".to_string(),
    )
    .sign(&sender_keys)
    .expect("Failed to sign event");

    println!("Sending first event (should trigger notifications)...");
    let response = client
        .post("http://127.0.0.1:3030/events")
        .json(&event)
        .send()
        .await
        .expect("Failed to send request");
    assert_eq!(response.status(), StatusCode::OK);

    // Wait for notifications
    sleep(Duration::from_millis(200)).await;

    // Verify first notifications were received
    let pushes_count_first = received_pushes.lock().await.len();
    let webhooks_count_first = received_webhooks.lock().await.len();
    assert_eq!(
        pushes_count_first, 1,
        "Should receive first push notification"
    );
    assert_eq!(
        webhooks_count_first, 1,
        "Should receive first webhook notification"
    );

    println!(
        "✅ First notifications received: {} push, {} webhook",
        pushes_count_first, webhooks_count_first
    );

    // Clear notifications for second attempt
    received_pushes.lock().await.clear();
    received_webhooks.lock().await.clear();

    // Send the SAME event again (should still trigger notifications since server hasn't restarted)
    println!("Sending same event again before restart (should be blocked by in-memory cache)...");
    let response = client
        .post("http://127.0.0.1:3030/events")
        .json(&event)
        .send()
        .await
        .expect("Failed to send request");
    assert_eq!(response.status(), StatusCode::OK);

    sleep(Duration::from_millis(200)).await;

    // After restart, the same event should NOT trigger notifications due to persistent seen_events
    let pushes_count_second = received_pushes.lock().await.len();
    let webhooks_count_second = received_webhooks.lock().await.len();
    assert_eq!(
        pushes_count_second, 0,
        "Should NOT receive duplicate push notification before restart"
    );
    assert_eq!(
        webhooks_count_second, 0,
        "Should NOT receive duplicate webhook notification before restart"
    );

    println!("✅ No duplicate notifications before restart (blocked by persistence)");
}

async fn test_social_graph_filtered_notifications(
    client: &Client,
    push_port: u16,
    webhook_port: u16,
    received_pushes: Arc<Mutex<Vec<serde_json::Value>>>,
    received_webhooks: Arc<Mutex<Vec<serde_json::Value>>>,
) {
    received_pushes.lock().await.clear();
    received_webhooks.lock().await.clear();

    let (root_keys, _) = get_test_keys_pair(11);
    let (subscriber_keys, allowed_author_keys) = get_test_keys_pair(12);
    let (_, muted_author_keys) = get_test_keys_pair(13);
    let (_, spam_author_keys) = get_test_keys_pair(14);
    let subscriber_pubkey = subscriber_keys.public_key().to_string();

    let browser_keys = generate_browser_keys();
    let push_subscription = create_push_subscription(
        &browser_keys,
        &format!("http://127.0.0.1:{}/push", push_port),
    );

    let new_subscription = serde_json::json!({
        "webhooks": [format!("http://127.0.0.1:{}/webhook", webhook_port)],
        "web_push_subscriptions": [push_subscription],
        "social_graph_filter": true,
        "filter": {
            "#p": [subscriber_pubkey.clone()],
            "kinds": [1]
        }
    });

    let response = make_authed_request_with_keys(
        client,
        Method::POST,
        "http://127.0.0.1:3030/subscriptions",
        Some(new_subscription),
        &subscriber_keys,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let contact_list_event = UnsignedEvent::new(
        root_keys.public_key(),
        Timestamp::now(),
        Kind::ContactList,
        vec![Tag::public_key(allowed_author_keys.public_key())],
        String::new(),
    )
    .sign(&root_keys)
    .expect("Failed to sign contact list event");

    let response = client
        .post("http://127.0.0.1:3030/events")
        .json(&contact_list_event)
        .send()
        .await
        .expect("Failed to send contact list event");
    assert_eq!(response.status(), StatusCode::OK);

    let mute_list_event = UnsignedEvent::new(
        subscriber_keys.public_key(),
        Timestamp::now(),
        Kind::from(10_000),
        vec![Tag::public_key(muted_author_keys.public_key())],
        String::new(),
    )
    .sign(&subscriber_keys)
    .expect("Failed to sign mute list event");

    let response = client
        .post("http://127.0.0.1:3030/events")
        .json(&mute_list_event)
        .send()
        .await
        .expect("Failed to send mute list event");
    assert_eq!(response.status(), StatusCode::OK);

    sleep(Duration::from_millis(150)).await;

    let spam_event = create_test_event(&spam_author_keys, &subscriber_pubkey);
    let response = client
        .post("http://127.0.0.1:3030/events")
        .json(&spam_event)
        .send()
        .await
        .expect("Failed to send spam event");
    assert_eq!(response.status(), StatusCode::OK);

    sleep(Duration::from_millis(150)).await;
    assert_eq!(
        received_pushes.lock().await.len(),
        0,
        "Out-of-graph author should not trigger push notification"
    );
    assert_eq!(
        received_webhooks.lock().await.len(),
        0,
        "Out-of-graph author should not trigger webhook notification"
    );

    received_pushes.lock().await.clear();
    received_webhooks.lock().await.clear();

    let muted_event = create_test_event(&muted_author_keys, &subscriber_pubkey);
    let response = client
        .post("http://127.0.0.1:3030/events")
        .json(&muted_event)
        .send()
        .await
        .expect("Failed to send muted author event");
    assert_eq!(response.status(), StatusCode::OK);

    sleep(Duration::from_millis(150)).await;
    assert_eq!(
        received_pushes.lock().await.len(),
        0,
        "Muted author should not trigger push notification"
    );
    assert_eq!(
        received_webhooks.lock().await.len(),
        0,
        "Muted author should not trigger webhook notification"
    );

    received_pushes.lock().await.clear();
    received_webhooks.lock().await.clear();

    let allowed_event = create_test_event(&allowed_author_keys, &subscriber_pubkey);
    let response = client
        .post("http://127.0.0.1:3030/events")
        .json(&allowed_event)
        .send()
        .await
        .expect("Failed to send allowed author event");
    assert_eq!(response.status(), StatusCode::OK);

    sleep(Duration::from_millis(150)).await;
    assert_eq!(
        received_pushes.lock().await.len(),
        1,
        "In-graph author should trigger push notification"
    );
    assert_eq!(
        received_webhooks.lock().await.len(),
        1,
        "In-graph author should trigger webhook notification"
    );
}

async fn test_muted_authors_are_blocked_without_social_graph_filter(
    client: &Client,
    push_port: u16,
    webhook_port: u16,
    received_pushes: Arc<Mutex<Vec<serde_json::Value>>>,
    received_webhooks: Arc<Mutex<Vec<serde_json::Value>>>,
) {
    received_pushes.lock().await.clear();
    received_webhooks.lock().await.clear();

    let (subscriber_keys, muted_author_keys) = get_test_keys_pair(17);
    let subscriber_pubkey = subscriber_keys.public_key().to_string();

    let browser_keys = generate_browser_keys();
    let push_subscription = create_push_subscription(
        &browser_keys,
        &format!("http://127.0.0.1:{}/push", push_port),
    );

    let new_subscription = serde_json::json!({
        "webhooks": [format!("http://127.0.0.1:{}/webhook", webhook_port)],
        "web_push_subscriptions": [push_subscription],
        "social_graph_filter": false,
        "filter": {
            "#p": [subscriber_pubkey.clone()],
            "kinds": [1]
        }
    });

    let response = make_authed_request_with_keys(
        client,
        Method::POST,
        "http://127.0.0.1:3030/subscriptions",
        Some(new_subscription),
        &subscriber_keys,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let mute_list_event = UnsignedEvent::new(
        subscriber_keys.public_key(),
        Timestamp::now(),
        Kind::from(10_000),
        vec![Tag::public_key(muted_author_keys.public_key())],
        String::new(),
    )
    .sign(&subscriber_keys)
    .expect("Failed to sign mute list event");

    let response = client
        .post("http://127.0.0.1:3030/events")
        .json(&mute_list_event)
        .send()
        .await
        .expect("Failed to send mute list event");
    assert_eq!(response.status(), StatusCode::OK);

    sleep(Duration::from_millis(150)).await;

    let muted_event = create_test_event(&muted_author_keys, &subscriber_pubkey);
    let response = client
        .post("http://127.0.0.1:3030/events")
        .json(&muted_event)
        .send()
        .await
        .expect("Failed to send muted author event");
    assert_eq!(response.status(), StatusCode::OK);

    sleep(Duration::from_millis(150)).await;
    assert_eq!(
        received_pushes.lock().await.len(),
        0,
        "Muted author should not trigger push notification when social graph filtering is disabled"
    );
    assert_eq!(
        received_webhooks.lock().await.len(),
        0,
        "Muted author should not trigger webhook notification when social graph filtering is disabled"
    );
}

async fn test_push_rate_limit_allows_burst_and_throttles_push_targets(
    client: &Client,
    push_port: u16,
    webhook_port: u16,
    received_pushes: Arc<Mutex<Vec<serde_json::Value>>>,
    received_webhooks: Arc<Mutex<Vec<serde_json::Value>>>,
) {
    received_pushes.lock().await.clear();
    received_webhooks.lock().await.clear();

    let (subscriber_keys, first_author_keys) = get_test_keys_pair(15);
    let (_, second_author_keys) = get_test_keys_pair(16);
    let (_, third_author_keys) = get_test_keys_pair(17);
    let subscriber_pubkey = subscriber_keys.public_key().to_string();

    let browser_keys = generate_browser_keys();
    let push_subscription = create_push_subscription(
        &browser_keys,
        &format!("http://127.0.0.1:{}/push", push_port),
    );

    let new_subscription = serde_json::json!({
        "webhooks": [format!("http://127.0.0.1:{}/webhook", webhook_port)],
        "web_push_subscriptions": [push_subscription],
        "filter": {
            "#p": [subscriber_pubkey.clone()],
            "kinds": [1]
        }
    });

    let response = make_authed_request_with_keys(
        client,
        Method::POST,
        "http://127.0.0.1:3030/subscriptions",
        Some(new_subscription),
        &subscriber_keys,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let first_event = create_test_event(&first_author_keys, &subscriber_pubkey);
    let response = client
        .post("http://127.0.0.1:3030/events")
        .json(&first_event)
        .send()
        .await
        .expect("Failed to send first rate limit test event");
    assert_eq!(response.status(), StatusCode::OK);

    sleep(Duration::from_millis(150)).await;

    let second_event = create_test_event(&second_author_keys, &subscriber_pubkey);
    let response = client
        .post("http://127.0.0.1:3030/events")
        .json(&second_event)
        .send()
        .await
        .expect("Failed to send second rate limit test event");
    assert_eq!(response.status(), StatusCode::OK);

    sleep(Duration::from_millis(150)).await;

    let third_event = create_test_event(&third_author_keys, &subscriber_pubkey);
    let response = client
        .post("http://127.0.0.1:3030/events")
        .json(&third_event)
        .send()
        .await
        .expect("Failed to send third rate limit test event");
    assert_eq!(response.status(), StatusCode::OK);

    sleep(Duration::from_millis(150)).await;

    assert_eq!(
        received_pushes.lock().await.len(),
        2,
        "Push delivery should allow the configured burst before throttling"
    );
    assert_eq!(
        received_webhooks.lock().await.len(),
        3,
        "Webhook delivery should remain unthrottled"
    );
}

#[tokio::test]
async fn test_integration() {
    let client = Client::new();
    let (push_port, received_pushes) = start_mock_push_server().await;
    let (webhook_port, received_webhooks) = start_mock_webhook_server().await;
    let (fcm_port, received_fcm) = start_mock_fcm_server().await;
    let (apns_port, received_apns) = start_mock_apns_server().await;
    let fcm_key_path = "test_db/firebase-key.json";
    let apns_key_path = "test_db/apns-auth-key.pem";
    write_mock_fcm_service_account(fcm_key_path, fcm_port);
    write_test_file(apns_key_path, TEST_APNS_AUTH_KEY_PEM);
    let mut child = start_server_with_extra_env(vec![
        (
            "NNS_FCM_SERVICE_ACCOUNT_KEY".to_string(),
            fcm_key_path.to_string(),
        ),
        (
            "NNS_FCM_API_BASE_URL".to_string(),
            format!("http://127.0.0.1:{}", fcm_port),
        ),
        ("NNS_APNS_KEY_ID".to_string(), "TESTKEY123".to_string()),
        ("NNS_APNS_TEAM_ID".to_string(), "TEAM123456".to_string()),
        ("NNS_APNS_TOPIC".to_string(), "to.iris".to_string()),
        ("NNS_APNS_ENVIRONMENT".to_string(), "sandbox".to_string()),
        ("NNS_APNS_AUTH_KEY".to_string(), apns_key_path.to_string()),
        (
            "NNS_APNS_API_BASE_URL".to_string(),
            format!("http://127.0.0.1:{}", apns_port),
        ),
    ])
    .await;

    // Clear any existing notifications before starting tests
    received_pushes.lock().await.clear();
    received_webhooks.lock().await.clear();
    received_fcm.lock().await.clear();
    received_apns.lock().await.clear();

    test_info_endpoint(&client).await;
    test_subscription_endpoints(&client, push_port, webhook_port).await;
    test_author_subscription_endpoints(&client, push_port, webhook_port).await;
    test_event_endpoint(
        &client,
        push_port,
        webhook_port,
        received_pushes.clone(),
        received_webhooks.clone(),
    )
    .await;
    test_encrypted_dm_notifications(
        &client,
        push_port,
        webhook_port,
        received_pushes.clone(),
        received_webhooks.clone(),
    )
    .await;
    test_failed_push_endpoint_removal(&client, push_port, webhook_port).await;
    test_subscription_cors(&client, push_port, webhook_port).await;
    test_subscription_author_update(
        &client,
        push_port,
        webhook_port,
        received_pushes.clone(),
        received_webhooks.clone(),
    )
    .await;
    test_mobile_subscription_endpoints(&client).await;
    test_mobile_push_delivery(&client, received_fcm.clone(), received_apns).await;
    test_mobile_push_token_moves_between_subscriptions(&client, received_fcm).await;
    test_seen_events_persistence(
        &client,
        push_port,
        webhook_port,
        received_pushes.clone(),
        received_webhooks.clone(),
    )
    .await;
    test_auth_failures(&client, push_port, webhook_port).await;
    test_muted_authors_are_blocked_without_social_graph_filter(
        &client,
        push_port,
        webhook_port,
        received_pushes.clone(),
        received_webhooks.clone(),
    )
    .await;

    // Send SIGTERM to the child process
    child
        .kill()
        .await
        .expect("Failed to send signal to process");
    child
        .wait()
        .await
        .expect("Failed to wait for process to exit");

    let (graph_root_keys, _) = get_test_keys_pair(11);

    let mut child = start_server_with_extra_env(vec![(
        "NNS_SOCIAL_GRAPH_ROOT_PUBKEY".to_string(),
        graph_root_keys.public_key().to_string(),
    )])
    .await;

    test_social_graph_filtered_notifications(
        &client,
        push_port,
        webhook_port,
        received_pushes.clone(),
        received_webhooks.clone(),
    )
    .await;

    child
        .kill()
        .await
        .expect("Failed to send signal to process");
    child
        .wait()
        .await
        .expect("Failed to wait for process to exit");

    let mut child = start_server_with_extra_env(vec![
        ("NNS_PUSH_RATE_LIMIT_BURST".to_string(), "2".to_string()),
        (
            "NNS_PUSH_RATE_LIMIT_REFILL_SECONDS".to_string(),
            "60".to_string(),
        ),
    ])
    .await;

    test_push_rate_limit_allows_burst_and_throttles_push_targets(
        &client,
        push_port,
        webhook_port,
        received_pushes,
        received_webhooks,
    )
    .await;

    child
        .kill()
        .await
        .expect("Failed to send signal to process");
    child
        .wait()
        .await
        .expect("Failed to wait for process to exit");

    // Clean up test directory after test
    let _ = fs::remove_dir_all("test_db");
}

// Move the test function here but make it a regular async function
async fn test_subscription_cors(client: &Client, push_port: u16, webhook_port: u16) {
    let (subscriber_keys, _) = get_test_keys_pair(7);
    let pubkey = subscriber_keys.public_key().to_string();

    // Generate browser keys for web push
    let browser_keys = generate_browser_keys();
    let push_subscription = create_push_subscription(
        &browser_keys,
        &format!("http://127.0.0.1:{}/push", push_port),
    );

    let new_subscription = serde_json::json!({
        "webhooks": [format!("http://127.0.0.1:{}/webhook", webhook_port)],
        "web_push_subscriptions": [push_subscription],
        "filter": {
            "#p": [pubkey]
        }
    });

    // 1. Test POST new subscription with CORS
    let response = make_authed_request_with_keys(
        &client,
        Method::POST,
        "http://127.0.0.1:3030/subscriptions",
        Some(new_subscription),
        &subscriber_keys,
    )
    .await;

    assert_eq!(response.status(), StatusCode::CREATED);

    let created = response.json::<serde_json::Value>().await.unwrap();
    let subscription_id = created["id"].as_str().unwrap();

    // 2. Test GET subscription with CORS
    let get_url = format!("http://127.0.0.1:3030/subscriptions/{}", subscription_id);
    let response =
        make_authed_request_with_keys(&client, Method::GET, &get_url, None, &subscriber_keys).await;

    assert_eq!(response.status(), StatusCode::OK);

    // 3. Test OPTIONS preflight for subscription update
    let response = client
        .request(Method::OPTIONS, &get_url)
        .header("Access-Control-Request-Method", "POST")
        .header(
            "Access-Control-Request-Headers",
            "authorization, content-type",
        )
        .header("Origin", "*")
        .send()
        .await
        .expect("Failed to send preflight OPTIONS request");

    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers();
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .unwrap()
            .to_str()
            .unwrap(),
        "*"
    );
    assert!(
        headers
            .get("access-control-allow-methods")
            .unwrap()
            .to_str()
            .unwrap()
            .split(",")
            .any(|m| m.trim() == "POST"),
        "Should allow POST method"
    );
    assert!(
        headers
            .get("access-control-allow-headers")
            .unwrap()
            .to_str()
            .unwrap()
            .split(",")
            .any(|h| h.trim().eq_ignore_ascii_case("authorization")),
        "Should allow Authorization header"
    );
}
