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

#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};

use crate::db::DbHandler;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    env_logger::init();

    debug!("Starting main function");

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
