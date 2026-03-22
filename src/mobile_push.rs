use std::error::Error;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use log::{debug, warn};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::Settings;
use crate::notifications::NotificationPayload;

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

    let aps_body = if payload.body.is_empty() {
        json!({ "title": payload.title })
    } else {
        json!({ "title": payload.title, "body": payload.body })
    };
    let request_body = json!({
        "aps": {
            "alert": aps_body,
            "sound": "default",
            "mutable-content": 1
        },
        "event": payload.event,
        "title": payload.title,
        "body": payload.body,
        "icon": payload.icon,
        "url": payload.url
    });

    let base_url = resolve_apns_api_base_url(settings);
    let endpoint = format!("{}/3/device/{}", base_url.trim_end_matches('/'), token);
    debug!("Sending APNS push for token {}", abbreviate_token(token));
    let response = reqwest::Client::new()
        .post(endpoint)
        .header("authorization", format!("bearer {}", jwt))
        .header("apns-topic", topic)
        .header("apns-push-type", "alert")
        .header("apns-priority", "10")
        .json(&request_body)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    if status.is_success() {
        return Ok(false);
    }

    let should_remove = should_remove_apns_token(status, &body);
    warn!(
        "APNS push failed for token {} with status {}: {}",
        abbreviate_token(token),
        status,
        body
    );
    Ok(should_remove)
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
