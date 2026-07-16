//! Shared control-plane plumbing for the management submodules: the [`ApiError`] envelope
//! every non-2xx response wears, [`api_error`], the [`ApiJson`] extractor that keeps axum's own
//! rejections in that envelope, and a small re-export prelude of the axum/serde/utoipa vocabulary
//! the handler modules share. Split out of the `mgmt` facade (plan §W5).

use axum::extract::Request;

// Re-export prelude: the vocabulary every handler submodule pulls in via `use super::shared::*`.
pub(crate) use super::MgmtState;
pub(crate) use axum::extract::{Path, Query, State};
pub(crate) use axum::http::StatusCode;
pub(crate) use axum::response::{IntoResponse, Response};
pub(crate) use axum::Json;
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use std::sync::Arc;
pub(crate) use utoipa::ToSchema;

/// Error envelope for every non-2xx response.
#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct ApiError {
    error: String,
}

pub(crate) fn api_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(ApiError {
            error: message.to_string(),
        }),
    )
        .into_response()
}

/// `axum::Json` whose rejections (bad JSON → 400/422, wrong content-type → 415) are
/// rewrapped in the [`ApiError`] envelope, keeping "every non-2xx body is `ApiError`" true.
pub(crate) struct ApiJson<T>(pub(crate) T);

impl<S, T> axum::extract::FromRequest<S> for ApiJson<T>
where
    Json<T>: axum::extract::FromRequest<S, Rejection = axum::extract::rejection::JsonRejection>,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(ApiJson(value)),
            Err(rejection) => Err(api_error(rejection.status(), &rejection.body_text())),
        }
    }
}
