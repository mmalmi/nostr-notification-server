use nostr_sdk::nostr::{Event, Kind, Tag};
use warp::reject;

#[derive(Debug)]
pub struct AuthError;
impl reject::Reject for AuthError {}

pub async fn verify_nostr_auth(
    auth_header: Option<String>,
    url: &str,
    method: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(auth) = auth_header {
        // Check if it starts with "Nostr "
        if !auth.starts_with("Nostr ") {
            return Err("Invalid authorization format".into());
        }

        let event_str = base64::decode(&auth[6..])
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        let event_json = String::from_utf8(event_str)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        
        let event: Event = serde_json::from_slice(&event_json.as_bytes())
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        // Verify event signature
        event.verify()
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        // Verify kind is 27235 (authorization event)
        if event.kind != Kind::from(27235) {
            return Err("Invalid event kind".into());
        }

        // Verify timestamp is within 10 minutes
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
            .as_secs() as u64;
        
        let event_time = event.created_at.as_u64();
        if event_time > current_time + 600 {
            return Err("Auth event timestamp too far in future".into());
        }
        if event_time < current_time - 600 {
            return Err("Auth event timestamp too old".into());
        }
        
        // Verify URL and method tags
        let url_tag = event.tags.iter().find(|tag| {
            matches!(Tag::parse(&["url", url]), Ok(t) if t == **tag)
        });

        let method_tag = event.tags.iter().find(|tag| {
            matches!(Tag::parse(&["method", method]), Ok(t) if t == **tag)
        });

        match (url_tag, method_tag) {
            (Some(_), Some(_)) => Ok(event.pubkey.to_string()),
            _ => Err("Missing or invalid URL/method tags".into())
        }
    } else {
        Err("Auth header not present".into())
    }
} 