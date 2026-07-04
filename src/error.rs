use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// Application-level error mapped to a JSON problem response.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("unauthorized")]
    Unauthorized,

    #[error("{0} not found")]
    NotFound(String),

    #[error("invalid request: {0}")]
    BadRequest(String),

    #[error("upstream service error: {0}")]
    Upstream(String),

    #[error("no suitable release found: {0}")]
    NoRelease(String),

    #[error("release is a compressed RAR archive and cannot be streamed")]
    CompressedRarUnsupported,

    #[error("release is an encrypted RAR archive and cannot be streamed")]
    EncryptedRarUnsupported,

    #[error("invalid RAR archive: {0}")]
    InvalidRarArchive(String),

    #[error("article missing on provider: {0}")]
    MissingSegment(String),

    #[error(transparent)]
    Database(#[from] sqlx::Error),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl AppError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Upstream(_) => StatusCode::BAD_GATEWAY,
            Self::NoRelease(_)
            | Self::CompressedRarUnsupported
            | Self::EncryptedRarUnsupported
            | Self::InvalidRarArchive(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::MissingSegment(_) => StatusCode::BAD_GATEWAY,
            Self::Database(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            tracing::error!(error = ?self, "request failed");
        }
        let body = Json(json!({
            "error": self.to_string(),
            "status": status.as_u16(),
        }));
        (status, body).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;
