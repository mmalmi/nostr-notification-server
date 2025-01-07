use warp::{Filter, Reply, Rejection};
use warp::path::FullPath;
use warp::http::{Method, StatusCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use log::{info, error, debug};
use nostr_sdk::Event;
use std::convert::Infallible;
use uuid::Uuid;
use warp::cors::Cors;

use crate::config::Settings;
use crate::db::DbHandler;
use crate::auth::verify_nostr_auth;
use crate::errors::{CustomRejection, clean_error_message};
use crate::notifications::handle_incoming_event;
use crate::subscription::Subscription;

// Add this function to create CORS configuration
fn cors() -> Cors {
    warp::cors()
        .allow_any_origin()
        .allow_headers(vec![
            "authorization",
            "content-type",
            "accept",
            "origin",
            "access-control-request-method",
            "access-control-request-headers",
            "sec-fetch-dest",
            "sec-fetch-mode",
            "sec-fetch-site",
            "sec-gpc",
            "priority",
            "accept-encoding",
            "accept-language",
            "content-length",
            "user-agent",
            "referer",
        ])
        .allow_methods(vec!["GET", "POST", "DELETE", "OPTIONS"])
        .max_age(86400) // 24 hours
        .expose_headers(vec!["content-type", "content-length"])
        .build()
}

pub async fn run_http_server(
    db_handler: Arc<DbHandler>,
    settings: Arc<Settings>,
    shutdown_flag: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let port = settings.http_port;
    info!("Starting HTTP server on port {}", port);

    let db_filter = with_db(db_handler.clone());
    let settings_filter = with_settings(settings.clone());

    let _log = warp::log::custom(|info| {
        info!("{} {} {} {}ms - {}", 
            info.method(),
            info.path(),
            info.status(),
            info.elapsed().as_millis(),
            info.remote_addr()
                .map_or("unknown".to_string(), |addr| addr.ip().to_string())
        );
    });

    let base_url = settings.base_url.clone();

    // Auth middleware
    let auth = warp::header::optional::<String>("authorization")
        .and(warp::path::full())
        .and(warp::method())
        .and_then(move |auth_header: Option<String>, path: FullPath, method: Method| {
            let base_url = base_url.clone();
            async move {
                // Skip auth check for OPTIONS requests
                if method == Method::OPTIONS {
                    return Ok(String::new());  // Empty string as pubkey for OPTIONS
                }

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
        .and(settings_filter.clone())
        .and(auth.clone());

    let get_subscriptions = subscriptions.clone()
        .and(warp::get())
        .and_then(handle_get_subscriptions);

    let get_subscription = subscriptions.clone()
        .and(warp::get())
        .and(warp::path::param::<String>())
        .and_then(handle_get_subscription);

    let update_subscription = subscriptions.clone()
        .and(warp::post())
        .and(warp::path::param::<String>())
        .and(warp::body::json())
        .and_then(handle_update_subscription);

    let post_subscription = subscriptions.clone()
        .and(warp::post())
        .and(warp::body::json())
        .and_then(handle_post_subscription);

    let delete_subscription = subscriptions
        .and(warp::delete())
        .and(warp::path::param::<String>())
        .and_then(handle_delete_subscription);

    let info = warp::path("info")
        .and(warp::get())
        .and(with_settings(settings.clone()))
        .and_then(handle_info);

    let events = warp::path("events")
        .and(db_filter.clone())
        .and(settings_filter.clone())
        .and(warp::post())
        .and(warp::body::json())
        .and_then(handle_post_event);

    let root = warp::path::end()
        .and(warp::get())
        .and(with_db(db_handler.clone()))
        .and_then(handle_root);

    let routes = root
        .or(get_subscription)
        .or(get_subscriptions)
        .or(update_subscription)
        .or(post_subscription)
        .or(delete_subscription)
        .or(events)
        .or(info)
        .with(cors())
        .boxed()
        .recover(crate::errors::handle_rejection);

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

async fn handle_get_subscriptions(
    db: Arc<DbHandler>,
    _settings: Arc<Settings>,
    auth_pubkey: String,
) -> Result<impl warp::Reply, warp::Rejection> {
    match db.get_subscriptions_for_pubkey(&auth_pubkey) {
        Ok(subscriptions) => {
            let response: serde_json::Map<String, serde_json::Value> = subscriptions.into_iter()
                .map(|(id, sub)| (id, serde_json::to_value(sub).unwrap()))
                .collect();
            Ok(warp::reply::json(&response))
        }
        Err(e) => {
            error!("Database error while getting subscriptions: {}", e);
            Err(warp::reject::custom(CustomRejection {
                message: "Database error".to_string(),
                error_type: "DatabaseError".to_string(),
            }))
        }
    }
}

async fn handle_get_subscription(
    db: Arc<DbHandler>,
    _settings: Arc<Settings>,
    auth_pubkey: String,
    id: String,
) -> Result<impl warp::Reply, warp::Rejection> {
    match db.get_subscription(&auth_pubkey, &id) {
        Ok(Some(subscription)) => {
            Ok(warp::reply::json(&subscription))
        }
        Ok(None) => {
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
    mut subscription: Subscription,
) -> Result<impl Reply, Rejection> {
    let id = Uuid::new_v4().to_string();
    
    // Set the subscriber to match the authenticated user
    subscription.subscriber = auth_pubkey.clone();
    
    db_handler.save_subscription(&auth_pubkey, &id, &subscription)
        .map_err(|e| {
            error!("Database error saving new subscription: {}", e);
            warp::reject::custom(CustomRejection {
                message: clean_error_message(&e.to_string()),
                error_type: "DatabaseError".to_string(),
            })
        })?;

    Ok(warp::reply::with_status(
        warp::reply::json(&serde_json::json!({
            "id": id,
            "status": "Created"
        })),
        StatusCode::CREATED
    ))
}

async fn handle_delete_subscription(
    db_handler: Arc<DbHandler>,
    _settings: Arc<Settings>,
    auth_pubkey: String,
    id: String,
) -> Result<impl Reply, Rejection> {
    match db_handler.delete_subscription(&auth_pubkey, &id) {
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
        if let Err(e) = handle_incoming_event(&event, db_handler, settings.as_ref()).await {
            error!("Background event processing failed: {:?}", e);
        }
    });

    Ok(warp::reply::with_status(
        warp::reply::json(&"Event accepted"),
        StatusCode::OK
    ))
}

async fn handle_info(
    settings: Arc<Settings>,
) -> Result<impl Reply, Rejection> {
    Ok(warp::reply::json(&serde_json::json!({
        "vapid_public_key": settings.vapid_public_key,
    })))
}

async fn handle_root(
    db: Arc<DbHandler>,
) -> Result<impl Reply, Rejection> {
    let stats = db.get_stats().map_err(|e| {
        error!("Database error getting stats: {}", e);
        warp::reject::custom(CustomRejection {
            message: "Database error".to_string(),
            error_type: "DatabaseError".to_string(),
        })
    })?;

    Ok(warp::reply::json(&serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "subscriptions": stats.subscriptions,
        "social_graph": stats.social_graph,
        "profiles": stats.profiles,
    })))
}

async fn handle_update_subscription(
    db_handler: Arc<DbHandler>,
    _settings: Arc<Settings>,
    auth_pubkey: String,
    id: String,
    mut subscription: Subscription,
) -> Result<impl Reply, Rejection> {
    // Set the subscriber to match the authenticated user
    subscription.subscriber = auth_pubkey.clone();
    
    db_handler.save_subscription(&auth_pubkey, &id, &subscription)
        .map_err(|e| {
            error!("Database error updating subscription: {}", e);
            warp::reject::custom(CustomRejection {
                message: clean_error_message(&e.to_string()),
                error_type: "DatabaseError".to_string(),
            })
        })?;

    Ok(warp::reply::with_status(
        warp::reply::json(&serde_json::json!({
            "status": "Updated"
        })),
        StatusCode::OK
    ))
}

async fn shutdown_signal(shutdown_flag: Arc<AtomicBool>) {
    while !shutdown_flag.load(Ordering::Relaxed) {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
}

// Helper function to share db with routes
fn with_db(db: Arc<DbHandler>) -> impl Filter<Extract = (Arc<DbHandler>,), Error = Infallible> + Clone {
    warp::any().map(move || db.clone())
}

// Helper function to share settings with routes
fn with_settings(settings: Arc<Settings>) -> impl Filter<Extract = (Arc<Settings>,), Error = Infallible> + Clone {
    warp::any().map(move || settings.clone())
}
