use regex::Regex;
use std::fmt;
use warp::http::StatusCode;
use warp::reject;
use warp::Rejection;
use log;

#[derive(Debug)]
pub struct CustomRejection {
    pub message: String,
    pub error_type: String,
}

impl reject::Reject for CustomRejection {}

impl fmt::Display for CustomRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.error_type, self.message)
    }
}

pub async fn handle_rejection(err: Rejection) -> Result<impl warp::Reply, std::convert::Infallible> {
    let (code, body) = if err.find::<CustomRejection>().is_some() {
        let e = err.find::<CustomRejection>().unwrap();
        log::info!("Custom rejection: {} - {}", e.error_type, e.message);
        match e.error_type.as_str() {
            "AuthError" => (
                StatusCode::UNAUTHORIZED,
                serde_json::json!({
                    "error": "UNAUTHORIZED",
                    "message": e.message
                })
            ),
            _ => (
                StatusCode::BAD_REQUEST,
                serde_json::json!({
                    "error": e.error_type,
                    "message": e.message
                })
            )
        }
    } else if err.find::<warp::reject::MissingHeader>().is_some() {
        log::info!("Missing header error");
        (
            StatusCode::UNAUTHORIZED,
            serde_json::json!({
                "error": "UNAUTHORIZED",
                "message": "Missing authorization header"
            })
        )
    } else if err.is_not_found() || err.find::<warp::reject::MethodNotAllowed>().is_some() {
        log::info!("Not Found error");
        (
            StatusCode::NOT_FOUND,
            serde_json::json!({
                "error": "NOT_FOUND",
                "message": "Resource not found"
            })
        )
    } else if err.find::<warp::body::BodyDeserializeError>().is_some() {
        log::info!("Body deserialization error");
        (
            StatusCode::BAD_REQUEST,
            serde_json::json!({
                "error": "INVALID_BODY",
                "message": "Invalid request body"
            })
        )
    } else {
        log::error!("Unhandled rejection: {:?}", err);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({
                "error": "INTERNAL_SERVER_ERROR",
                "message": "Internal server error"
            })
        )
    };

    log::info!("Sending error response: {} - {}", code.as_u16(), body);
    
    Ok(warp::reply::with_status(
        warp::reply::json(&body),
        code
    ))
}

pub fn clean_error_message(message: &str) -> String {
    // Remove line and column information
    let re = Regex::new(r" at line \d+ column \d+").unwrap();
    let cleaned = re.replace_all(message, "").into_owned();

    // Extract the most relevant part of the error message
    if let Some(index) = cleaned.find("Invalid public key") {
        cleaned[index..].to_string()
    } else {
        "Invalid request body".to_string()
    }
} 