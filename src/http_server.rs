use warp::{Filter, Reply, Rejection};
use warp::path::FullPath;
use warp::http::{Method, StatusCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use log::{info, error, debug, warn};
use nostr_sdk::Event;

use crate::config::Settings;
use crate::db::DbHandler;
use crate::auth::verify_nostr_auth;
use crate::errors::{CustomRejection, clean_error_message};
use crate::notifications::handle_incoming_event;
use crate::subscription::Subscription;

pub async fn run_http_server(
    db_handler: Arc<DbHandler>,
    settings: Arc<Settings>,
    shutdown_flag: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let port = settings.http_port;
    info!("Starting HTTP server on port {}", port);

    let settings_filter = Arc::clone(&settings);
    let settings_auth = Arc::clone(&settings);

    let db_filter = warp::any().map(move || db_handler.clone());

    let settings = warp::any().map(move || Arc::clone(&settings_filter));
    
    let log = warp::log::custom(|info| {
        info!("{} {} {} {}ms - {}", 
            info.method(),
            info.path(),
            info.status(),
            info.elapsed().as_millis(),
            info.remote_addr()
                .map_or("unknown".to_string(), |addr| addr.ip().to_string())
        );
    });

    // Auth middleware
    let auth = warp::header::optional::<String>("authorization")
        .and(warp::path::full())
        .and(warp::method())
        .and_then(move |auth_header: Option<String>, path: FullPath, method: Method| {
            let base_url = settings_auth.base_url();
            async move {
                // First check if auth header exists
                let auth_str = auth_header.ok_or_else(|| {
                    warp::reject::custom(CustomRejection {
                        message: "Missing authorization header".to_string(),
                        error_type: "AuthError".to_string(),
                    })
                })?;

                // Then verify the auth
                match verify_nostr_auth(
                    Some(auth_str),
                    &format!("{}{}", base_url, path.as_str()),
                    method.as_str(),
                ).await {
                    Ok(pubkey) => {
                        Ok(pubkey)
                    },
                    Err(e) => {
                        info!("Auth failed: {:?}", e);
                        Err(warp::reject::custom(CustomRejection {
                            message: "Invalid authorization".to_string(),
                            error_type: "AuthError".to_string(),
                        }))
                    }
                }
            }
        });

    let subscriptions = warp::path("subscriptions")
        .and(db_filter.clone())
        .and(settings.clone())
        .and(auth.clone());

    let get_subscription = subscriptions.clone()
        .and(warp::get())
        .and(warp::path::param::<String>())
        .and_then(handle_get_subscription);

    let post_subscription = subscriptions.clone()
        .and(warp::post())
        .and(warp::path::param::<String>())
        .and(warp::body::json())
        .and_then(handle_post_subscription);

    let delete_subscription = subscriptions
        .and(warp::delete())
        .and(warp::path::param::<String>())
        .and_then(handle_delete_subscription);

    let info = warp::path("info")
        .and(settings.clone())
        .and(warp::get())
        .and_then(handle_info);

    let events = warp::path("events")
        .and(db_filter.clone())
        .and(settings)
        .and(warp::post())
        .and(warp::body::json())
        .and_then(handle_post_event);

    let root = warp::path::end()
        .and(db_filter.clone())
        .and(warp::get())
        .and_then(handle_root);

    let routes = root
        .or(get_subscription)
        .or(post_subscription)
        .or(delete_subscription)
        .or(events)
        .or(info)
        .recover(crate::errors::handle_rejection)
        .with(log)
        .with(warp::cors()
            .allow_any_origin()
            .allow_headers(vec!["content-type", "authorization"])
            .allow_methods(vec!["GET", "POST", "DELETE", "OPTIONS"]));

    // Create server
    let (addr, server) = warp::serve(routes)
        .bind_with_graceful_shutdown(
            ([0, 0, 0, 0], port),
            shutdown_signal(shutdown_flag)
        );

    info!("Server running on http://{}", addr);
    server.await;
    info!("HTTP server shutdown complete");

    Ok(())
}

async fn handle_get_subscription(
    db: Arc<DbHandler>,
    _settings: Arc<Settings>,
    auth_pubkey: String,
    pubkey: String,
) -> Result<impl warp::Reply, warp::Rejection> {
    debug!("handle_get_subscription called with auth_pubkey: {}, requested pubkey: {}", 
        auth_pubkey, pubkey);

    if auth_pubkey != pubkey {
        warn!("Auth pubkey {} does not match requested pubkey {}", 
            auth_pubkey, pubkey);
        return Err(warp::reject::custom(CustomRejection {
            message: "Not authorized to access this subscription".to_string(),
            error_type: "AuthError".to_string(),
        }));
    }

    match db.get_subscription(&pubkey) {
        Ok(Some(subscription)) => {
            debug!("Found subscription for pubkey: {}", pubkey);
            Ok(warp::reply::json(&subscription))
        }
        Ok(None) => {
            debug!("No subscription found for pubkey: {}", pubkey);
            Err(warp::reject::not_found())
        }
        Err(e) => {
            error!("Database error while getting subscription: {}", e);
            Err(warp::reject::custom(CustomRejection {
                message: "Database error".to_string(),
                error_type: "DatabaseError".to_string(),
            }))
        }
    }
}

async fn handle_post_subscription(
    db_handler: Arc<DbHandler>,
    _settings: Arc<Settings>,
    auth_pubkey: String,
    pubkey: String,
    subscription: Subscription,
) -> Result<impl Reply, Rejection> {
    debug!("POST subscription request - auth_pubkey: {}, pubkey: {}, subscription: {:?}", 
        auth_pubkey, pubkey, subscription);

    // Verify the authenticated user matches the requested pubkey
    if auth_pubkey != pubkey {
        return Err(warp::reject::custom(CustomRejection {
            message: "Unauthorized".to_string(),
            error_type: "AuthError".to_string(),
        }));
    }

    let existing = db_handler.get_subscription(&pubkey).map_err(|e| {
        error!("Database error getting subscription: {}", e);
        warp::reject::custom(CustomRejection {
            message: clean_error_message(&e.to_string()),
            error_type: "DatabaseError".to_string(),
        })
    })?;

    match existing {
        Some(mut existing_sub) => {
            existing_sub.merge(subscription);
            
            db_handler.save_subscription(&pubkey, &existing_sub).map_err(|e| {
                error!("Database error saving subscription: {}", e);
                warp::reject::custom(CustomRejection {
                    message: clean_error_message(&e.to_string()),
                    error_type: "DatabaseError".to_string(),
                })
            })?;

            Ok(warp::reply::with_status(
                warp::reply::json(&"Updated"),
                StatusCode::OK
            ))
        },
        None => {
            db_handler.save_subscription(&pubkey, &subscription).map_err(|e| {
                error!("Database error saving new subscription: {}", e);
                warp::reject::custom(CustomRejection {
                    message: clean_error_message(&e.to_string()),
                    error_type: "DatabaseError".to_string(),
                })
            })?;

            Ok(warp::reply::with_status(
                warp::reply::json(&"Created"),
                StatusCode::CREATED
            ))
        }
    }
}

async fn handle_delete_subscription(
    db_handler: Arc<DbHandler>,
    _settings: Arc<Settings>,
    auth_pubkey: String,
    pubkey: String,
) -> Result<impl Reply, Rejection> {
    if auth_pubkey != pubkey {
        return Err(warp::reject::custom(CustomRejection {
            message: "Unauthorized".to_string(),
            error_type: "AuthError".to_string(),
        }));
    }

    match db_handler.delete_subscription(&pubkey) {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err(warp::reject::not_found()),
        Err(e) => Err(warp::reject::custom(CustomRejection {
            message: clean_error_message(&e.to_string()),
            error_type: "DatabaseError".to_string(),
        })),
    }
}

async fn handle_post_event(
    db_handler: Arc<DbHandler>,
    settings: Arc<Settings>,
    event: Event,
) -> Result<impl Reply, Rejection> {
    debug!("Received event: {:?}", event);
    
    // Measure signature verification time
    let start = std::time::Instant::now();
    let verify_result = event.verify();
    let verification_time = start.elapsed();
    debug!("Event signature verification took {:?}", verification_time);

    // Verify event signature
    if let Err(e) = verify_result {
        debug!("Event signature verification failed: {:?}", e);
        return Err(warp::reject::custom(CustomRejection {
            message: "Invalid event signature".to_string(),
            error_type: "ValidationError".to_string(),
        }));
    }

    // Spawn a new task to handle the event processing
    tokio::spawn(async move {
        if let Err(e) = handle_incoming_event(&event, &db_handler, settings.as_ref()).await {
            error!("Background event processing failed: {:?}", e);
        }
    });

    Ok(warp::reply::with_status(
        warp::reply::json(&"Event accepted"),
        StatusCode::OK
    ))
}

async fn handle_info(
    settings: Arc<Settings>
) -> Result<impl Reply, Rejection> {
    Ok(warp::reply::json(&serde_json::json!({
        "vapid_public_key": settings.vapid_public_key
    })))
}

async fn handle_root(
    db: Arc<DbHandler>,
) -> Result<impl Reply, Rejection> {
    let rtxn = db.env.read_txn().map_err(|e| {
        error!("Database error getting stats: {}", e);
        warp::reject::custom(CustomRejection {
            message: "Database error".to_string(),
            error_type: "DatabaseError".to_string(),
        })
    })?;

    let stats = db.db.stat(&rtxn).map_err(|e| {
        error!("Database error getting stats: {}", e);
        warp::reject::custom(CustomRejection {
            message: "Database error".to_string(),
            error_type: "DatabaseError".to_string(),
        })
    })?;

    Ok(warp::reply::json(&serde_json::json!({
        "subscriptions": stats.entries
    })))
}

async fn shutdown_signal(shutdown_flag: Arc<AtomicBool>) {
    while !shutdown_flag.load(Ordering::Relaxed) {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
}
