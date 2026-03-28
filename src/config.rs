use crate::vapid;
use config::{Config, ConfigError, Environment, File};
use log::{debug, error};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct Settings {
    pub http_port: u16,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(skip)]
    pub vapid_public_key: String,
    #[serde(skip)]
    pub vapid_private_key: String,
    #[serde(default = "default_db_path")]
    pub db_path: String,
    #[serde(default)]
    pub relays: Vec<String>,
    pub db_map_size: usize,
    pub icon_url: String,
    pub notification_base_url: String,
    pub social_graph_root_pubkey: String,
    #[serde(default = "default_use_social_graph")]
    pub use_social_graph: bool,
    #[serde(default)]
    pub social_graph_snapshot_path: Option<String>,
    #[serde(default = "default_max_seen_events")]
    pub max_seen_events: usize,
    #[serde(default = "default_max_p_tags")]
    pub max_p_tags: usize,
    #[serde(default = "default_push_min_interval_seconds")]
    pub push_min_interval_seconds: u64,
    #[serde(default)]
    pub fcm_service_account_key: Option<String>,
    #[serde(default = "default_fcm_api_base_url")]
    pub fcm_api_base_url: String,
    #[serde(default)]
    pub apns_key_id: Option<String>,
    #[serde(default)]
    pub apns_team_id: Option<String>,
    #[serde(default)]
    pub apns_topic: Option<String>,
    #[serde(default = "default_apns_environment")]
    pub apns_environment: String,
    #[serde(default)]
    pub apns_auth_key: Option<String>,
    #[serde(default = "default_apns_api_base_url")]
    pub apns_api_base_url: String,
}

fn default_db_path() -> String {
    "db".to_string()
}

fn default_base_url() -> String {
    "http://localhost:3030".to_string()
}

fn default_use_social_graph() -> bool {
    true
}

fn default_max_seen_events() -> usize {
    1_000_000
}

fn default_max_p_tags() -> usize {
    10
}

fn default_push_min_interval_seconds() -> u64 {
    0
}

fn default_fcm_api_base_url() -> String {
    "https://fcm.googleapis.com".to_string()
}

fn default_apns_environment() -> String {
    "production".to_string()
}

fn default_apns_api_base_url() -> String {
    String::new()
}

impl Settings {
    pub fn new() -> Result<Self, ConfigError> {
        let mut s = Config::builder();

        // Load default config file
        s = s
            .add_source(File::with_name("config/default"))
            .add_source(File::with_name("config/local").required(false));

        // Set default values
        s = s.set_default("db_path", "db")?;
        s = s.set_default("db_map_size", 1024 * 1024 * 1024)?;
        s = s.set_default("relays", Vec::<String>::new())?;
        s = s.set_default("icon_url", "https://iris.to/img/android-chrome-192x192.png")?;
        s = s.set_default("notification_base_url", "https://iris.to")?;
        s = s.set_default(
            "social_graph_root_pubkey",
            "npub1g53mukxnjkcmr94fhryzkqutdz2ukq4ks0gvy5af25rgmwsl4ngq43drvk",
        )?;
        s = s.set_default("use_social_graph", true)?;
        s = s.set_default("max_seen_events", 1_000_000)?;
        s = s.set_default("max_p_tags", 10)?;
        s = s.set_default("push_min_interval_seconds", 0)?;
        s = s.set_default("fcm_api_base_url", "https://fcm.googleapis.com")?;
        s = s.set_default("apns_environment", "production")?;
        s = s.set_default("apns_api_base_url", "")?;

        // Parse comma-separated relays from environment
        if let Ok(relays_str) = std::env::var("NNS_RELAYS") {
            let relays: Vec<String> = relays_str
                .split(',')
                .map(|s| s.trim().to_string())
                .collect();
            if !relays.is_empty() {
                s = s.set_override("relays", relays)?;
            }
        }

        // Add other environment variables
        s = s.add_source(Environment::with_prefix("NNS"));

        // Keep port default
        s = s.set_default("http_port", 3030)?;

        // Add base_url default
        s = s.set_default("base_url", "http://localhost:3030")?;

        let config = s.build()?;
        let mut settings: Settings = config.try_deserialize()?;

        // Ensure VAPID keys exist and load them
        let (private_key, public_key) = vapid::ensure_vapid_keys().map_err(|e| {
            error!("Failed to load VAPID keys: {}", e);
            ConfigError::Message(e.to_string())
        })?;

        debug!("Loaded VAPID public key: {}", public_key);
        settings.vapid_private_key = private_key;
        settings.vapid_public_key = public_key;

        Ok(settings)
    }
}
