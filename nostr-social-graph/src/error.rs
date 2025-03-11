use std::error::Error;
use std::fmt;
use std::num::ParseIntError;
use crate::unique_ids::UniqueIdError;

#[derive(Debug)]
pub enum SocialGraphError {
    Database(String),
    InvalidData(String),
    IO(std::io::Error),
    Parse(String),
}

impl fmt::Display for SocialGraphError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Database(msg) => write!(f, "Database error: {}", msg),
            Self::InvalidData(msg) => write!(f, "Invalid data: {}", msg),
            Self::IO(err) => write!(f, "IO error: {}", err),
            Self::Parse(msg) => write!(f, "Parse error: {}", msg),
        }
    }
}

impl Error for SocialGraphError {}

impl From<std::io::Error> for SocialGraphError {
    fn from(err: std::io::Error) -> Self {
        Self::IO(err)
    }
}

impl From<heed::Error> for SocialGraphError {
    fn from(err: heed::Error) -> Self {
        Self::Database(err.to_string())
    }
}

impl From<ParseIntError> for SocialGraphError {
    fn from(err: ParseIntError) -> Self {
        Self::Parse(err.to_string())
    }
}

impl From<Box<dyn Error>> for SocialGraphError {
    fn from(err: Box<dyn Error>) -> Self {
        Self::InvalidData(err.to_string())
    }
}

impl From<Box<dyn Error + Send + Sync>> for SocialGraphError {
    fn from(err: Box<dyn Error + Send + Sync>) -> Self {
        Self::InvalidData(err.to_string())
    }
}

impl From<UniqueIdError> for SocialGraphError {
    fn from(error: UniqueIdError) -> Self {
        SocialGraphError::Database(error.to_string())
    }
} 