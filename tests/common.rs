#![allow(dead_code)]

use base64::encode_config;
use base64::URL_SAFE_NO_PAD;
use bytes;
use nostr_sdk::nostr::{Event, Keys, Kind, SecretKey, Tag, Timestamp, UnsignedEvent};
use p256::{ecdh::EphemeralSecret, PublicKey};
use rand::RngCore;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::sleep;
use warp::Filter;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WebPushSubscription {
    pub endpoint: String,
    pub auth: String,
    pub p256dh: String,
}

pub const TEST_FCM_PROJECT_ID: &str = "iris-test-project";
pub const TEST_FCM_PRIVATE_KEY_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvwIBADANBgkqhkiG9w0BAQEFAASCBKkwggSlAgEAAoIBAQDAIs7IlPZSpagrl1WM
jJvUeqxVq/3NO59XYJX7uOGjfttr9fzxx0n56XL/OnQyCZ7Z4oHM7p0sxF14AEcLh9v5
Q7J3+paE4rXwMcyn8hX2RyvPAj6HmKsONcLfwkxW7j1mBvGM6AF4ypJAxPX12QbtQZLE
y4Cd3F1nxe0uvQpXKwLpPR0Dd7BoJSMw1yMjkuX/406/5JWigLHzar4Ayt8jhhOrfx4x
yLwXGQJxxJreIWkaJGrTDHc390YAAuzn5ZVMRwUenmKtaNiUb2LxQtUmlFtqWD+pVbAg
VtudZ/Vf4F/7vIm1HuZrdKwTPA2YQfSCHsZhPq/X52T5ZFKZ7D6tAgMBAAECggEBALjB
v+6Jr8QRoAEq6QzaOQ69V/KaDNjJuJOhQRhp1DAP0JekV7N3W9+BaR+c6hcjwSjr8r1J
xsJBoU+/lJG19wVe38MXqJ3oE+QBPzdQR2YnUi0hj1d8qyBi+h2JDTeaqFfv3V8NyjyJ
LtIxlALwywRixeuPdQJX4UnkvgrvpX3jJvpFjOYFqllu7xVZbv5YV76slNph2/lUB7PJ
Gmmb4h5+bXiv2v9rfw3ytS75nV3SOlEeQAOMwi1Mkpufdpp451yYIuUNV7LF3OrhH5mB
TqCRNPadDA0KaS1JGZlM+tsMMslPLUxxEyxUEl3CiPRlNbDKn4jk9vCeIxm22w5hqQEC
gYEA67aV03NWgrqaiU0ozcpaFj5VTLQSOYEtymFLq71/vN5dGtdzpByadKQMEWo5ZIJR
LNw1W6X5s7+MpbyAkAe7b5QMKfziVXz2kNSVH1dIWIdcCW4++O1xKHAEB77RF29o//3K
GcZQj7rZMzhUHaC/dDZRVqy5alnoFNNrbKToQNECgYEA0KwYTRtjQsXDX1PYLDUaf8A8
u73lzNXQy9GNry7Ph8AlIy0htN+2Sa+WIj2Cz3DjyDY6xi3ZoC+UAxq7iIdAzpZiu441
7kiV9sMouDE3/O5ih0gWFKmzRViabKW1QGgw/nG6Ym5+6n/lfZ/zoB/hF9+E5nxlKhtD
4lsQv5awNx0CgYAkVyoSR5322b4pnPPFhoUNGN8dzEVjCD9/DDEWcUjYXZANK1pw2tgV
U5Voue/PRygsumafkp9EzytoAf/wNMD5GuIlNw/ODk4VVjEHe/VzcKsH6S9cQX9ItLxq
VUj3S/3sObyG7MRO5IfIFc8iIj5iNF2l90s+0k2tqErPnT0RgQKBgQDKSpuAXJWcjLV6
+4AsUwquYAFAi7Z0Ha+9dxeghYPAeUBHWqA7hUhlJKgp53GhgjH/zLqrlpVL2fPmEotM
rrnfzCBI7HNR3eIrh0Q5U9WQCNVRikuFmoHlLyD9RKNync8pS71BYRb+ZCBo6aA3UdBX
4WMoQd2ctTPZAyk4Ym/P7QKBgQCr4Fe6/1lmh7mzssn535MJIhqYRvwilAArsqaYYacy
iPKxYukgVnpginNH/U/P9/Vi/kxnQ1o37t76Hu7nI5o3k9ukUu/PBdz4UyvaseXqm/Jq
V547MSkT0ng3S97/X9Jwul7KPQ3JsOTbWZQhcQ+rY+jA/5THRy8vfpvMzfUnVQ==
-----END PRIVATE KEY-----"#;

pub const TEST_APNS_AUTH_KEY_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgteXr4MOrgdH24JEk
tlK4174VirxgMGBIEnTKekbUAuahRANCAARuB6SGNmolhcYCxNqEQNVfqRJAhpam
Xlng61e27oQ1/haR54V5BDazORFlO1DXJofc40N7iRqYnw7zGLBdzmZh
-----END PRIVATE KEY-----"#;

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

    start_server_without_cleanup().await
}

pub async fn start_server_without_cleanup() -> tokio::process::Child {
    start_server_with_extra_env(Vec::new()).await
}

pub async fn start_server_with_extra_env(
    extra_env: Vec<(String, String)>,
) -> tokio::process::Child {
    // Use a unique DB path so reruns do not pick up stale LMDB state from a
    // previously crashed server process.
    fs::create_dir_all("test_db").ok();
    let unique_db_path = format!(
        "test_db/{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after UNIX_EPOCH")
            .as_nanos()
    );
    fs::create_dir_all(&unique_db_path).expect("Failed to create unique test database directory");

    let mut command = Command::new("cargo");
    command
        .arg("run")
        .env("NNS_DB_PATH", &unique_db_path)
        .env("NNS_HOSTNAME", "0.0.0.0")
        .env("NNS_BASE_URL", "http://127.0.0.1:3030")
        .env("RUST_LOG", "warn,nostr_notification_server=debug")
        .kill_on_drop(true);
    for (key, value) in extra_env {
        command.env(key, value);
    }

    let child = command.spawn().expect("Failed to start application");

    // Give the server some time to start
    sleep(Duration::from_secs(2)).await;
    child
}

pub async fn make_authed_request(
    client: &Client,
    method: reqwest::Method,
    url: &str,
    json_body: Option<serde_json::Value>,
) -> reqwest::Response {
    let auth_header = create_auth_header(url, method.as_str()).await;

    let mut request = client
        .request(method, url)
        .header("authorization", auth_header);

    if let Some(body) = json_body {
        request = request.json(&body);
    }

    request.send().await.expect("Failed to send request")
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
            warp::reply::with_status(warp::reply::json(&"{}"), warp::http::StatusCode::CREATED)
        });

    // Add a catch-all route that returns 404
    let not_found = warp::any().map(|| {
        warp::reply::with_status(
            warp::reply::json(&"Not Found"),
            warp::http::StatusCode::NOT_FOUND,
        )
    });

    let routes = push_route.or(not_found);

    let (addr, server) = warp::serve(routes).bind_ephemeral(([127, 0, 0, 1], 0));

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
                println!(
                    ">>> Webhook received: {}",
                    serde_json::to_string_pretty(&body).unwrap()
                );
                webhooks.lock().await.push(body);
            });
            warp::reply::with_status(warp::reply::json(&"{}"), warp::http::StatusCode::OK)
        });

    let (addr, server) = warp::serve(routes).bind_ephemeral(([127, 0, 0, 1], 0));

    println!("🚀 Mock webhook server started on port {}", addr.port());
    tokio::spawn(server);
    (addr.port(), received_webhooks)
}

pub async fn start_mock_fcm_server() -> (u16, Arc<Mutex<Vec<serde_json::Value>>>) {
    let received_messages = Arc::new(Mutex::new(Vec::new()));
    let received_messages_clone = received_messages.clone();

    let token_route = warp::post().and(warp::path("token")).map(|| {
        warp::reply::json(&serde_json::json!({
            "access_token": "test-fcm-access-token",
            "token_type": "Bearer",
            "expires_in": 3600,
        }))
    });

    let send_route = warp::post()
        .and(warp::path("v1"))
        .and(warp::path("projects"))
        .and(warp::path::param::<String>())
        .and(warp::path("messages:send"))
        .and(warp::header::headers_cloned())
        .and(warp::body::json())
        .map(
            move |project_id: String, headers: warp::http::HeaderMap, body: serde_json::Value| {
                let messages = received_messages_clone.clone();
                tokio::spawn(async move {
                    messages.lock().await.push(serde_json::json!({
                        "project_id": project_id,
                        "headers": headers
                            .iter()
                            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                            .collect::<HashMap<String, String>>(),
                        "body": body,
                    }));
                });
                warp::reply::with_status(
                    warp::reply::json(&serde_json::json!({
                        "name": "projects/iris-test-project/messages/mock-message"
                    })),
                    warp::http::StatusCode::OK,
                )
            },
        );

    let routes = token_route.or(send_route);

    let (addr, server) = warp::serve(routes).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);
    (addr.port(), received_messages)
}

pub async fn start_mock_apns_server() -> (u16, Arc<Mutex<Vec<serde_json::Value>>>) {
    let received_pushes = Arc::new(Mutex::new(Vec::new()));
    let received_pushes_clone = received_pushes.clone();

    let route = warp::post()
        .and(warp::path("3"))
        .and(warp::path("device"))
        .and(warp::path::param::<String>())
        .and(warp::header::headers_cloned())
        .and(warp::body::json())
        .map(
            move |token: String, headers: warp::http::HeaderMap, body: serde_json::Value| {
                let pushes = received_pushes_clone.clone();
                tokio::spawn(async move {
                    pushes.lock().await.push(serde_json::json!({
                        "token": token,
                        "headers": headers
                            .iter()
                            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                            .collect::<HashMap<String, String>>(),
                        "body": body,
                    }));
                });
                warp::reply::with_status(
                    warp::reply::json(&serde_json::json!({})),
                    warp::http::StatusCode::OK,
                )
            },
        );

    let (addr, server) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);
    (addr.port(), received_pushes)
}

pub fn write_test_file(path: impl AsRef<Path>, contents: &str) {
    if let Some(parent) = path.as_ref().parent() {
        fs::create_dir_all(parent).expect("Failed to create test file parent directory");
    }
    fs::write(path, contents).expect("Failed to write test file");
}

pub fn write_mock_fcm_service_account(path: impl AsRef<Path>, token_port: u16) {
    let payload = serde_json::json!({
        "type": "service_account",
        "project_id": TEST_FCM_PROJECT_ID,
        "private_key_id": "test-private-key-id",
        "private_key": TEST_FCM_PRIVATE_KEY_PEM,
        "client_email": format!("firebase-adminsdk@test-{}.iam.gserviceaccount.com", token_port),
        "client_id": "1234567890",
        "token_uri": format!("http://127.0.0.1:{}/token", token_port),
    });
    write_test_file(path, &serde_json::to_string_pretty(&payload).unwrap());
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

    let event = unsigned_event.sign(keys).expect("Failed to sign event");

    format!(
        "Nostr {}",
        base64::encode(serde_json::to_string(&event).unwrap())
    )
}

// Private helper function needed by create_auth_header
fn get_test_keys() -> Keys {
    let secret_key =
        SecretKey::from_hex("1234567890123456789012345678901234567890123456789012345678901234")
            .expect("Invalid secret key");
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

    request.send().await.expect("Failed to send request")
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
        vec![Tag::public_key(
            nostr_sdk::PublicKey::from_hex(recipient_pubkey).unwrap(),
        )],
        "Test content".to_string(),
    );

    unsigned_event
        .sign(sender_keys)
        .expect("Failed to sign event")
}
