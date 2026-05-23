#[derive(Debug)]
pub enum Error {
    BadRequest(String),
    Unauthorized(String),
    NotFound(String),
    PaymentRequired(String),
    Internal(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::BadRequest(msg) => write!(f, "Bad Request: {}", msg),
            Error::Unauthorized(msg) => write!(f, "Unauthorized: {}", msg),
            Error::NotFound(msg) => write!(f, "Not Found: {}", msg),
            Error::PaymentRequired(msg) => write!(f, "Payment Required: {}", msg),
            Error::Internal(msg) => write!(f, "Internal Error: {}", msg),
        }
    }
}

impl std::error::Error for Error {}
