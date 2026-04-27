use std::error::Error;
use std::fs;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use log::{debug, warn};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::Settings;
use crate::notifications::{EventPayload, NotificationPayload};

#[derive(Debug, Deserialize)]
struct FcmServiceAccount {
    project_id: String,
    private_key: String,
    client_email: String,
    token_uri: String,
}

#[derive(Debug, Deserialize)]
struct FcmTokenResponse {
    access_token: String,
}

#[derive(Debug, Serialize)]
struct GoogleServiceAccountClaims<'a> {
    iss: &'a str,
    sub: &'a str,
    aud: &'a str,
    scope: &'a str,
    iat: u64,
    exp: u64,
}

#[derive(Debug, Serialize)]
struct ApnsClaims<'a> {
    iss: &'a str,
    iat: u64,
}

pub async fn send_fcm_push(
    token: &str,
    payload: &NotificationPayload,
    settings: &Settings,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let account = match load_fcm_service_account(settings)? {
        Some(account) => account,
        None => return Ok(false),
    };

    let access_token = fetch_fcm_access_token(&account).await?;
    let base_url = settings.fcm_api_base_url.trim_end_matches('/');
    let endpoint = format!(
        "{}/v1/projects/{}/messages:send",
        base_url, account.project_id
    );
    let event_json = serde_json::to_string(&payload.event)?;
    let request_body = json!({
        "message": {
            "token": token,
            "data": {
                "event": event_json,
                "title": payload.title,
                "body": payload.body,
                "icon": payload.icon,
                "url": payload.url,
            },
            "android": {
                "priority": "HIGH"
            }
        }
    });

    debug!("Sending FCM push for token {}", abbreviate_token(token));
    let response = reqwest::Client::new()
        .post(endpoint)
        .bearer_auth(access_token)
        .json(&request_body)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    if status.is_success() {
        return Ok(false);
    }

    let should_remove = should_remove_fcm_token(status, &body);
    warn!(
        "FCM push failed for token {} with status {}: {}",
        abbreviate_token(token),
        status,
        body
    );
    Ok(should_remove)
}

pub async fn send_apns_push(
    token: &str,
    payload: &NotificationPayload,
    settings: &Settings,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let key_id = match trimmed_non_empty(settings.apns_key_id.as_deref()) {
        Some(value) => value.to_string(),
        None => return Ok(false),
    };
    let team_id = match trimmed_non_empty(settings.apns_team_id.as_deref()) {
        Some(value) => value.to_string(),
        None => return Ok(false),
    };
    let topic = match trimmed_non_empty(settings.apns_topic.as_deref()) {
        Some(value) => value.to_string(),
        None => return Ok(false),
    };
    let auth_key_pem = match load_secret_value(&settings.apns_auth_key)? {
        Some(value) => value,
        None => return Ok(false),
    };

    let now = unix_now()?;
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(key_id);
    let jwt = encode(
        &header,
        &ApnsClaims {
            iss: &team_id,
            iat: now,
        },
        &EncodingKey::from_ec_pem(auth_key_pem.as_bytes())?,
    )?;

    let request_body = build_apns_request_body(payload);
    let request_body_bytes = serde_json::to_vec(&request_body)?;
    let payload_size = request_body_bytes.len();

    let base_url = resolve_apns_api_base_url(settings);
    let parsed_base_url = reqwest::Url::parse(&base_url)?;
    let endpoint = format!("{}/3/device/{}", base_url.trim_end_matches('/'), token);
    let host = parsed_base_url
        .host_str()
        .ok_or("APNS base URL is missing a host")?
        .to_string();
    let client = build_apns_client(&host, parsed_base_url.scheme() == "https")?;

    debug!(
        "Sending APNS push for token {} with payload_bytes={}",
        abbreviate_token(token),
        payload_size
    );
    let request = client
        .post(&endpoint)
        .header("authorization", format!("bearer {}", jwt))
        .header("apns-topic", topic)
        .header("apns-push-type", "alert")
        .header("apns-priority", "10")
        .header("content-type", "application/json")
        .body(request_body_bytes);
    let response = request.send().await.map_err(|error| {
        format!(
            "APNS request failed for token {} (payload_bytes={}, connect={}, timeout={}): {}",
            abbreviate_token(token),
            payload_size,
            error.is_connect(),
            error.is_timeout(),
            describe_error_chain(&error),
        )
    })?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    if status.is_success() {
        return Ok(false);
    }

    let should_remove = should_remove_apns_token(status, &body);
    warn!(
        "APNS push failed for token {} with status {} (payload_bytes={}): {}",
        abbreviate_token(token),
        status,
        payload_size,
        body
    );
    Ok(should_remove)
}

fn build_apns_request_body(payload: &NotificationPayload) -> serde_json::Value {
    json!({
        "aps": {
            "alert": {
                "title": payload.title,
                "body": payload.body
            },
            "mutable-content": 1
        },
        "event": compact_event_payload_for_apns(&payload.event),
        "title": payload.title,
        "body": payload.body,
        "icon": payload.icon,
        "url": payload.url
    })
}

fn compact_event_payload_for_apns(event: &EventPayload) -> serde_json::Value {
    match event {
        EventPayload::Full(event) => json!({
            "id": event.id.to_hex(),
            "pubkey": event.pubkey.to_hex(),
            "created_at": event.created_at.as_u64(),
            "kind": event.kind.as_u16(),
            "tags": event
                .tags
                .iter()
                .filter(|tag| tag.as_slice().first().map_or(false, |value| value == "header"))
                .map(|tag| tag.clone().to_vec())
                .collect::<Vec<Vec<String>>>(),
            "content": event.content,
            "sig": event.sig.to_string(),
        }),
        EventPayload::Details(details) => json!({
            "id": details.id,
            "pubkey": details.author,
            "kind": details.kind,
        }),
    }
}

fn build_apns_client(
    host: &str,
    require_http2: bool,
) -> Result<reqwest::Client, Box<dyn Error + Send + Sync>> {
    let socket_addrs = resolve_ipv4_socket_addrs(host, 443)?;
    let mut builder = reqwest::Client::builder();
    if !socket_addrs.is_empty() && host.parse::<std::net::IpAddr>().is_err() {
        builder = builder.resolve_to_addrs(host, &socket_addrs);
    }
    if require_http2 {
        builder = builder.http2_prior_knowledge();
    }
    Ok(builder.build()?)
}

fn resolve_ipv4_socket_addrs(
    host: &str,
    port: u16,
) -> Result<Vec<SocketAddr>, Box<dyn Error + Send + Sync>> {
    Ok((host, port)
        .to_socket_addrs()?
        .filter(|addr| addr.is_ipv4())
        .collect())
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

fn load_fcm_service_account(
    settings: &Settings,
) -> Result<Option<FcmServiceAccount>, Box<dyn Error + Send + Sync>> {
    let raw = match load_secret_value(&settings.fcm_service_account_key)? {
        Some(value) => value,
        None => return Ok(None),
    };
    let account = serde_json::from_str::<FcmServiceAccount>(&raw)?;
    Ok(Some(account))
}

async fn fetch_fcm_access_token(
    account: &FcmServiceAccount,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let now = unix_now()?;
    let claims = GoogleServiceAccountClaims {
        iss: &account.client_email,
        sub: &account.client_email,
        aud: &account.token_uri,
        scope: "https://www.googleapis.com/auth/firebase.messaging https://www.googleapis.com/auth/cloud-platform",
        iat: now,
        exp: now + 3600,
    };
    let jwt = encode(
        &Header::new(Algorithm::RS256),
        &claims,
        &EncodingKey::from_rsa_pem(account.private_key.as_bytes())?,
    )?;

    let response = reqwest::Client::new()
        .post(&account.token_uri)
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", jwt.as_str()),
        ])
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(format!("Failed to fetch FCM access token: {}", body).into());
    }
    let token = serde_json::from_str::<FcmTokenResponse>(&body)?;
    Ok(token.access_token)
}

fn resolve_apns_api_base_url(settings: &Settings) -> String {
    let configured = settings.apns_api_base_url.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    match settings
        .apns_environment
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "sandbox" => "https://api.sandbox.push.apple.com".to_string(),
        _ => "https://api.push.apple.com".to_string(),
    }
}

fn load_secret_value(raw: &Option<String>) -> Result<Option<String>, Box<dyn Error + Send + Sync>> {
    let Some(raw) = raw.as_ref() else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let path = Path::new(trimmed);
    if path.exists() {
        return Ok(Some(fs::read_to_string(path)?));
    }
    Ok(Some(trimmed.to_string()))
}

fn should_remove_fcm_token(status: StatusCode, body: &str) -> bool {
    if matches!(status, StatusCode::NOT_FOUND | StatusCode::GONE) {
        return true;
    }
    let normalized = body.to_ascii_lowercase();
    normalized.contains("unregistered")
        || normalized.contains("invalid registration token")
        || normalized.contains("not a valid fcm registration token")
}

fn should_remove_apns_token(status: StatusCode, body: &str) -> bool {
    if matches!(status, StatusCode::NOT_FOUND | StatusCode::GONE) {
        return true;
    }
    let normalized = body.to_ascii_lowercase();
    normalized.contains("baddevicetoken") || normalized.contains("unregistered")
}

fn trimmed_non_empty(value: Option<&str>) -> Option<&str> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn unix_now() -> Result<u64, Box<dyn Error + Send + Sync>> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn abbreviate_token(token: &str) -> String {
    if token.len() <= 12 {
        return token.to_string();
    }
    format!("{}...{}", &token[..8], &token[token.len() - 4..])
}

#[cfg(test)]
mod tests {
    use super::{build_apns_request_body, compact_event_payload_for_apns};
    use crate::notifications::{EventPayload, NotificationPayload};
    use nostr_sdk::nostr::Event;
    use nostr_sdk::JsonUtil;
    use serde_json::json;

    fn sample_event_payload() -> EventPayload {
        EventPayload::Full(
            Event::from_json(
                r#"{"id":"4b68ab3847feda7d6c62c1fbcbeebfa35eab7351ed5e78f4ddadea5df64b8015","pubkey":"f86c3c5d6a6924e0f1ff8d3cf6fcb09f5dcba238c2f1a68d0f4638b1cc403b4a","created_at":1700000000,"kind":1060,"tags":[],"content":"hello","sig":"4d7c79de55b59de3f3eb89e7dfc30ec443b13ac0b8e75cfd40c1d0acf7d500d7657ef0627c054b022cb35c50a6481d948e57843b4374c1c1ff717a8ba4a89822"}"#,
            )
            .expect("sample event should parse"),
        )
    }

    #[test]
    fn compact_event_payload_keeps_decryptable_fields_for_apns() {
        let compacted = compact_event_payload_for_apns(&sample_event_payload());

        assert_eq!(
            compacted["id"].as_str().unwrap(),
            "4b68ab3847feda7d6c62c1fbcbeebfa35eab7351ed5e78f4ddadea5df64b8015"
        );
        assert_eq!(
            compacted["pubkey"].as_str().unwrap(),
            "f86c3c5d6a6924e0f1ff8d3cf6fcb09f5dcba238c2f1a68d0f4638b1cc403b4a"
        );
        assert_eq!(compacted["kind"].as_u64().unwrap(), 1060);
        assert_eq!(compacted["content"].as_str().unwrap(), "hello");
        assert_eq!(compacted["tags"], json!([]));
        assert!(compacted["sig"].as_str().unwrap().len() >= 128);
    }

    #[test]
    fn build_apns_request_body_uses_visible_mutable_payload() {
        let payload = NotificationPayload {
            event: sample_event_payload(),
            title: "DM by Someone".to_string(),
            body: "New message".to_string(),
            icon: "https://example.com/icon.png".to_string(),
            url: "https://example.com/note".to_string(),
        };

        let request_body = build_apns_request_body(&payload);

        assert_eq!(
            request_body["aps"],
            json!({
                "alert": {
                    "title": "DM by Someone",
                    "body": "New message"
                },
                "mutable-content": 1
            })
        );
        assert_eq!(
            request_body["event"]["id"].as_str().unwrap(),
            "4b68ab3847feda7d6c62c1fbcbeebfa35eab7351ed5e78f4ddadea5df64b8015"
        );
        assert_eq!(request_body["event"]["kind"].as_u64().unwrap(), 1060);
        assert_eq!(request_body["event"]["content"].as_str().unwrap(), "hello");
    }
}
