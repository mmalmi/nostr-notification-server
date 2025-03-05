use web_push::{
    VapidSignatureBuilder, 
    WebPushMessageBuilder,
    ContentEncoding,
    SubscriptionInfo,
    IsahcWebPushClient,
    WebPushClient,
    WebPushError,
};
use serde::{Deserialize, Serialize};
use log::{error, info, debug};
use crate::config::Settings;
use crate::notifications::{NotificationPayload, EventPayload};
use std::error::Error;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WebPushSubscription {
    pub endpoint: String,
    pub auth: String,
    pub p256dh: String,
}

impl WebPushSubscription {
    fn normalize_base64url(input: &str) -> String {
        let without_padding = input.trim_end_matches('=');
        
        without_padding
            .replace('+', "-")
            .replace('/', "_")
    }

    pub fn normalized(&self) -> Self {
        WebPushSubscription {
            endpoint: self.endpoint.clone(),
            auth: Self::normalize_base64url(&self.auth),
            p256dh: Self::normalize_base64url(&self.p256dh),
        }
    }
}

pub async fn send_web_push(
    subscription: &WebPushSubscription, 
    payload: &NotificationPayload,
    settings: &Settings,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let event_id = match &payload.event {
        EventPayload::Full(event) => event.id.to_string(),
        EventPayload::Details(details) => details.id.clone()
    };
    
    info!("Sending web push notification{}", 
        if event_id.is_empty() { String::new() } 
        else { format!(" for event: {}", event_id) }
    );
    
    let normalized = subscription.normalized();
    
    let subscription_info = SubscriptionInfo::new(
        &normalized.endpoint,
        &normalized.p256dh,
        &normalized.auth,
    );

    let content = serde_json::to_vec(payload)?;

    debug!("Creating VAPID signature builder");
    let sig_builder = VapidSignatureBuilder::from_pem(
        settings.vapid_private_key.as_bytes(),
        &subscription_info
    )?;

    debug!("Building web push message");
    let mut builder = WebPushMessageBuilder::new(&subscription_info);
    builder.set_payload(ContentEncoding::Aes128Gcm, &content);
    
    debug!("Building VAPID signature");
    let signature = sig_builder.build().map_err(|e| {
        error!("Failed to build VAPID signature: {}", e);
        e
    })?;
    builder.set_vapid_signature(signature);

    debug!("Building final web push message");
    let message = builder.build().map_err(|e| {
        error!("Failed to build web push message: {}", e);
        e
    })?;

    debug!("Sending push notification to endpoint: {}", subscription.endpoint);
    let client = IsahcWebPushClient::new()?;
    let result = client.send(message).await;

    match result {
        Ok(response) => {
            debug!("Push notification response: {:?}", response);
            info!("Web push notification sent successfully");
            Ok(false)
        }
        Err(e) => {
            let should_remove = matches!(e, 
                WebPushError::EndpointNotValid | 
                WebPushError::EndpointNotFound
            );
            info!("Failed to send push notification: {}", e);
            Ok(should_remove)
        }
    }
} 
