#![allow(dead_code)]

use tokio::process::Command;
use reqwest::Client;
use std::time::Duration;
use tokio::time::sleep;
use std::sync::Arc;
use tokio::sync::Mutex;
use warp::Filter;
use std::fs;
use std::collections::HashMap;
use bytes;
use nostr_sdk::nostr::{Keys, Kind, Tag, Timestamp, SecretKey, UnsignedEvent, Event};
use p256::{
    ecdh::EphemeralSecret,
    PublicKey,
};
use rand::RngCore;
use base64::encode_config;
use base64::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WebPushSubscription {
    pub endpoint: String,
    pub auth: String,
    pub p256dh: String,
}

pub fn get_test_keys_pair(modifier: u8) -> (Keys, Keys) {
    let base_key1 = "1234567890123456789012345678901234567890123456789012345678901234";
    let base_key2 = "4321432143214321432143214321432143214321432143214321432143214321";
    
    // Modify the last byte of each key based on the modifier
    let key1 = format!("{}{:02x}", &base_key1[..62], modifier);
    let key2 = format!("{}{:02x}", &base_key2[..62], modifier.wrapping_add(1));

    let secret_key1 = SecretKey::from_hex(&key1).expect("Invalid secret key");
    let secret_key2 = SecretKey::from_hex(&key2).expect("Invalid secret key");
    (Keys::new(secret_key1), Keys::new(secret_key2))
}

pub async fn start_server() -> tokio::process::Child {
    // Clean up any existing test directory
    let _ = fs::remove_dir_all("test_db");
    
    // Create fresh test directory
    fs::create_dir_all("test_db").expect("Failed to create test database directory");

    let child = Command::new("cargo")
        .arg("run")
        .env("NNS_DB_PATH", "test_db")
        .env("NNS_HOSTNAME", "0.0.0.0")
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to start application");

    // Give the server some time to start
    sleep(Duration::from_secs(2)).await;
    child
}

pub async fn make_authed_request(
    client: &Client, 
    method: reqwest::Method, 
    url: &str, 
    json_body: Option<serde_json::Value>
) -> reqwest::Response {
    let auth_header = create_auth_header(url, method.as_str()).await;
    
    let mut request = client
        .request(method, url)
        .header("authorization", auth_header);
    
    if let Some(body) = json_body {
        request = request.json(&body);
    }
    
    request.send()
        .await
        .expect("Failed to send request")
}

pub async fn start_mock_push_server() -> (u16, Arc<Mutex<Vec<serde_json::Value>>>) {
    let received_pushes = Arc::new(Mutex::new(Vec::new()));
    let received_pushes_clone = received_pushes.clone();

    let push_route = warp::post()
        .and(warp::path("push"))
        .and(warp::header::headers_cloned())
        .and(warp::body::bytes())
        .map(move |headers: warp::http::HeaderMap, body: bytes::Bytes| {
            let pushes = received_pushes_clone.clone();
            tokio::spawn(async move {
                // Store both headers and body for verification if needed
                let value = serde_json::json!({
                    "headers": headers
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                        .collect::<HashMap<String, String>>(),
                    "body": body.to_vec(),
                });
                pushes.lock().await.push(value);
            });
            
            // Return 201 Created as per Web Push protocol
            warp::reply::with_status(
                warp::reply::json(&"{}"),
                warp::http::StatusCode::CREATED,
            )
        });

    // Add a catch-all route that returns 404
    let not_found = warp::any()
        .map(|| {
            warp::reply::with_status(
                warp::reply::json(&"Not Found"),
                warp::http::StatusCode::NOT_FOUND,
            )
        });

    let routes = push_route.or(not_found);

    let (addr, server) = warp::serve(routes)
        .bind_ephemeral(([127, 0, 0, 1], 0));
    
    tokio::spawn(server);
    (addr.port(), received_pushes)
}

pub async fn start_mock_webhook_server() -> (u16, Arc<Mutex<Vec<serde_json::Value>>>) {
    let received_webhooks = Arc::new(Mutex::new(Vec::new()));
    let received_webhooks_clone = received_webhooks.clone();

    let routes = warp::post()
        .and(warp::body::json())
        .map(move |body: serde_json::Value| {
            let webhooks = received_webhooks_clone.clone();
            tokio::spawn(async move {
                println!(">>> Webhook received: {}", serde_json::to_string_pretty(&body).unwrap());
                webhooks.lock().await.push(body);
            });
            warp::reply::with_status(
                warp::reply::json(&"{}"),
                warp::http::StatusCode::OK,
            )
        });

    let (addr, server) = warp::serve(routes)
        .bind_ephemeral(([127, 0, 0, 1], 0));
    
    println!("🚀 Mock webhook server started on port {}", addr.port());
    tokio::spawn(server);
    (addr.port(), received_webhooks)
}

// Private helper function needed by make_authed_request
async fn create_auth_header(url: &str, method: &str) -> String {
    let keys = get_test_keys();
    create_auth_header_with_keys(url, method, &keys).await
}

async fn create_auth_header_with_keys(url: &str, method: &str, keys: &Keys) -> String {
    let tags = vec![
        Tag::parse(&["u", url]).expect("Failed to create url tag"),
        Tag::parse(&["method", method]).expect("Failed to create method tag"),
    ];

    let unsigned_event = UnsignedEvent::new(
        keys.public_key(),
        Timestamp::now(),
        Kind::from(27235),
        tags,
        String::new(),
    );

    let event = unsigned_event.sign(keys)
        .expect("Failed to sign event");

    format!("Nostr {}", base64::encode(serde_json::to_string(&event).unwrap()))
}

// Private helper function needed by create_auth_header
fn get_test_keys() -> Keys {
    let secret_key = SecretKey::from_hex(
        "1234567890123456789012345678901234567890123456789012345678901234"
    ).expect("Invalid secret key");
    Keys::new(secret_key)
}

pub async fn make_authed_request_with_keys(
    client: &Client, 
    method: reqwest::Method, 
    url: &str, 
    json_body: Option<serde_json::Value>,
    keys: &Keys,
) -> reqwest::Response {
    let auth_header = create_auth_header_with_keys(url, method.as_str(), keys).await;
    
    let mut request = client
        .request(method, url)
        .header("authorization", auth_header);
    
    if let Some(body) = json_body {
        request = request.json(&body);
    }
    
    request.send()
        .await
        .expect("Failed to send request")
}

pub struct BrowserKeys {
    pub auth_secret: [u8; 16],
    pub private_key: EphemeralSecret,
    pub public_key: PublicKey,
}

pub fn generate_browser_keys() -> BrowserKeys {
    // Generate random auth secret (16 bytes)
    let mut auth_secret = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut auth_secret);
    
    // Generate ECDH key pair
    let private_key = EphemeralSecret::random(&mut rand::thread_rng());
    let public_key = PublicKey::from(&private_key);

    BrowserKeys {
        auth_secret,
        private_key,
        public_key,
    }
}

pub fn create_push_subscription(browser_keys: &BrowserKeys, endpoint: &str) -> WebPushSubscription {
    WebPushSubscription {
        endpoint: endpoint.to_string(),
        p256dh: encode_config(browser_keys.public_key.to_sec1_bytes(), URL_SAFE_NO_PAD),
        auth: encode_config(browser_keys.auth_secret, URL_SAFE_NO_PAD),
    }
}

pub fn create_test_event(sender_keys: &Keys, recipient_pubkey: &str) -> Event {
    let unsigned_event = UnsignedEvent::new(
        sender_keys.public_key(),
        Timestamp::now(),
        Kind::from(1),
        vec![Tag::public_key(nostr_sdk::PublicKey::from_hex(recipient_pubkey).unwrap())],
        "Test content".to_string(),
    );

    unsigned_event.sign(sender_keys)
        .expect("Failed to sign event")
} 