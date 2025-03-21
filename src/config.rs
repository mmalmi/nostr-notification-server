use serde::Deserialize;
use config::{Config, ConfigError, File, Environment};
use crate::vapid;
use log::{debug, error};

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

impl Settings {
    pub fn new() -> Result<Self, ConfigError> {
        let mut s = Config::builder();

        // Load default config file
        s = s.add_source(File::with_name("config/default"))
            .add_source(File::with_name("config/local").required(false));

        // Set default values
        s = s.set_default("db_path", "db")?;
        s = s.set_default("db_map_size", 1024 * 1024 * 1024)?;
        s = s.set_default("relays", Vec::<String>::new())?;
        s = s.set_default("icon_url", "https://iris.to/img/android-chrome-192x192.png")?;
        s = s.set_default("notification_base_url", "https://iris.to")?;
        s = s.set_default("social_graph_root_pubkey", "npub1g53mukxnjkcmr94fhryzkqutdz2ukq4ks0gvy5af25rgmwsl4ngq43drvk")?;
        s = s.set_default("use_social_graph", true)?;

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
        let (private_key, public_key) = vapid::ensure_vapid_keys()
            .map_err(|e| {
                error!("Failed to load VAPID keys: {}", e);
                ConfigError::Message(e.to_string())
            })?;

        debug!("Loaded VAPID public key: {}", public_key);
        settings.vapid_private_key = private_key;
        settings.vapid_public_key = public_key;

        Ok(settings)
    }
}
