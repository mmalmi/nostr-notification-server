mod nostr_client;
mod http_server;
mod config;
mod vapid;
mod db;
mod subscription;
mod schema;
mod filter;
pub mod web_push;
pub mod auth;
pub mod errors;
pub mod notifications;

use std::sync::Arc;
use tokio;
use log::{info, error, debug};
use crate::config::Settings;
use std::time::Duration;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::signal;
use std::fs;
use clap::{Parser, Subcommand};
use env_logger;

#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};

use nostr_social_graph::Crawler;
use nostr_social_graph::SocialGraph;
use nostr_social_graph::ProfileHandler;
use nostr_social_graph::SerializedSocialGraph;
use crate::db::DbHandler;
use nostr_sdk::PublicKey;
use nostr_sdk::FromBech32;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the server normally
    Serve,
    /// Run the crawler only
    Crawl,
    /// Crawl profiles
    CrawlProfiles {
        /// Only crawl missing profiles
        #[arg(long)]
        missing_only: bool,
    },
    /// Import profiles from JSON file
    ImportProfiles {
        /// Path to JSON file containing profiles
        #[arg(short, long)]
        file: String,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialize the logger first thing
    env_logger::init();

    info!("Starting nostr-notification-server");
    debug!("Debug logging enabled");

    let cli = Cli::parse();

    match cli.command.unwrap_or(Commands::Serve) {
        Commands::Serve => run_server().await,
        Commands::Crawl => run_crawler().await,
        Commands::CrawlProfiles { missing_only } => run_profile_crawler(missing_only).await,
        Commands::ImportProfiles { file } => run_profile_import(&file).await,
    }
}

async fn run_server() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Load settings first
    let settings = Arc::new(Settings::new()?);
    debug!("Settings loaded");

    // Create db directory if it doesn't exist
    fs::create_dir_all(&settings.db_path).map_err(|e| {
        error!("Failed to create database directory: {:?}", e);
        e
    })?;

    info!("Initializing database...");
    let db_handler = Arc::new(DbHandler::new(&settings)?);
    info!("Database initialized successfully");

    // Create a single shutdown flag that we'll share
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_nostr = Arc::clone(&shutdown_flag);
    let shutdown_flag_http = Arc::clone(&shutdown_flag);

    let settings_clone = Arc::clone(&settings);
    let db_handler_clone = db_handler.clone();
    debug!("Spawning Nostr client");
    let nostr_client = tokio::spawn(async move {
        while !shutdown_flag_nostr.load(Ordering::Relaxed) {
            debug!("Starting Nostr client loop");
            let db = db_handler_clone.clone();
            let settings = Arc::clone(&settings_clone);
            let shutdown = Arc::clone(&shutdown_flag_nostr);

            match nostr_client::run_nostr_client(
                db,
                settings,
                shutdown,
            ).await {
                Ok(_) => {
                    info!("Nostr client completed successfully. Restarting...");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                },
                Err(e) => {
                    error!("Nostr client error: {}. Restarting in 5 seconds...", e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
        info!("Nostr client shutting down");
    });

    let settings_clone = Arc::clone(&settings);
    let db_handler_clone = db_handler.clone();
    debug!("Spawning HTTP server");
    let server = tokio::spawn(async move {
        debug!("HTTP server task started");
        if let Err(e) = http_server::run_http_server(
            db_handler_clone,
            settings_clone,
            shutdown_flag_http,
        ).await {
            error!("HTTP server error: {}", e);
        }
    });

    tokio::spawn(async move {
        shutdown_signal().await;
        info!("Shutdown signal received");
        shutdown_flag.store(true, Ordering::Relaxed);
    });

    debug!("Waiting for tasks to complete");
    tokio::try_join!(nostr_client, server)?;

    debug!("Main function ending");
    Ok(())
}

async fn run_crawler() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Load settings first
    let settings = Arc::new(Settings::new()?);
    debug!("Settings loaded");

    // Create db directory if it doesn't exist
    fs::create_dir_all(&settings.db_path).map_err(|e| {
        error!("Failed to create database directory: {:?}", e);
        e
    })?;

    // Create environment
    let env = unsafe {
        heed::EnvOpenOptions::new()
            .map_size(settings.db_map_size)
            .max_dbs(20)
            .open(&settings.db_path)?
    };

    // Convert npub to hex if needed
    let root_pubkey = &settings.social_graph_root_pubkey;
    let root_hex = if root_pubkey.starts_with("npub") {
        PublicKey::from_bech32(root_pubkey)?.to_hex()
    } else {
        root_pubkey.to_string()
    };

    // Create SocialGraph and ProfileHandler directly
    let social_graph = Arc::new(SocialGraph::new(
        &root_hex,
        env.clone(),
        None as Option<SerializedSocialGraph>
    )?);
    
    let profile_handler = Arc::new(ProfileHandler::new(env.clone())?);
    
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_clone = Arc::clone(&shutdown_flag);

    let crawler = Crawler::new(
        social_graph,
        profile_handler,
        settings.relays.clone(),
        shutdown_flag_clone.clone(),
    )?;

    tokio::spawn(async move {
        shutdown_signal().await;
        info!("Shutdown signal received");
        shutdown_flag.store(true, Ordering::Relaxed);
    });

    // Run initial crawl
    info!("Starting initial crawl");
    crawler.start().await?;

    info!("Crawler shutting down");
    Ok(())
}

async fn run_profile_crawler(missing_only: bool) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Load settings first
    let settings = Arc::new(Settings::new()?);
    debug!("Settings loaded");

    // Create db directory if it doesn't exist
    fs::create_dir_all(&settings.db_path).map_err(|e| {
        error!("Failed to create database directory: {:?}", e);
        e
    })?;

    // Create environment
    let env = unsafe {
        heed::EnvOpenOptions::new()
            .map_size(settings.db_map_size)
            .max_dbs(20)
            .open(&settings.db_path)?
    };

    // Convert npub to hex if needed
    let root_pubkey = &settings.social_graph_root_pubkey;
    let root_hex = if root_pubkey.starts_with("npub") {
        PublicKey::from_bech32(root_pubkey)?.to_hex()
    } else {
        root_pubkey.to_string()
    };

    let social_graph = Arc::new(SocialGraph::new(
        &root_hex,
        env.clone(),
        None as Option<SerializedSocialGraph>
    )?);
    
    let profile_handler = Arc::new(ProfileHandler::new(env.clone())?);
    
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_clone = Arc::clone(&shutdown_flag);

    let crawler = Crawler::new(
        social_graph,
        profile_handler,
        settings.relays.clone(),
        shutdown_flag_clone.clone(),
    )?;

    tokio::spawn(async move {
        shutdown_signal().await;
        info!("Shutdown signal received");
        shutdown_flag.store(true, Ordering::Relaxed);
    });

    info!("Starting profile crawl ({})", if missing_only { "missing only" } else { "all profiles" });
    crawler.crawl_profiles(missing_only).await?;

    info!("Profile crawler shutting down");
    Ok(())
}

async fn run_profile_import(file_path: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let settings = Arc::new(Settings::new()?);
    println!("Loading profiles from {}", file_path);

    let env = unsafe {
        heed::EnvOpenOptions::new()
            .map_size(settings.db_map_size)
            .max_dbs(20)
            .open(&settings.db_path)?
    };

    let profile_handler = Arc::new(ProfileHandler::new(env.clone())?);
    let file_content = fs::read_to_string(file_path)?;
    
    let before_count = profile_handler.get_profile_count()?;
    let imported = profile_handler.import_from_json(&file_content)?;
    let after_count = profile_handler.get_profile_count()?;

    println!("\nImport complete:");
    println!("  Profiles before: {}", before_count);
    println!("  Profiles imported: {}", imported);
    println!("  Total profiles now: {}", after_count);
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() {
    let mut sigterm = signal(SignalKind::terminate()).unwrap();
    tokio::select! {
        _ = signal::ctrl_c() => {
            println!("Ctrl+C received, shutting down...");
        }
        _ = sigterm.recv() => {
            println!("SIGTERM received, shutting down...");
        }
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    signal::ctrl_c().await.unwrap();
    println!("Ctrl+C received, shutting down...");
}
