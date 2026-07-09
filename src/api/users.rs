//! User management (Jellyfin-style): admins create and remove accounts;
//! everyone can see who they are. The owner (user 1) is implicit admin and
//! cannot be deleted.

use axum::extract::{Path, State};
use axum::{Extension, Json};
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::auth::{self, CurrentUser};
use crate::db::users::User;
use crate::error::{AppError, AppResult};
use crate::{db, state::AppState};

fn require_admin(user: &CurrentUser) -> AppResult<()> {
    if user.is_admin {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "only an admin can manage users".into(),
        ))
    }
}

/// All user accounts (admin only).
#[utoipa::path(get, path = "/users", tag = "users",
    responses((status = 200, body = [User]), (status = 401)))]
pub async fn list_users(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentUser>,
) -> AppResult<Json<Vec<User>>> {
    require_admin(&current)?;
    Ok(Json(db::users::list(&state.db).await?))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateUserRequest {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub is_admin: bool,
}

/// Create a user account (admin only).
#[utoipa::path(post, path = "/users", tag = "users",
    request_body = CreateUserRequest,
    responses((status = 200, body = User), (status = 400), (status = 401)))]
pub async fn create_user(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentUser>,
    Json(request): Json<CreateUserRequest>,
) -> AppResult<Json<User>> {
    require_admin(&current)?;
    let name = request.username.trim();
    if name.is_empty() || name.chars().count() > 60 {
        return Err(AppError::BadRequest(
            "username must be 1-60 characters".into(),
        ));
    }
    if request.password.chars().count() < 4 {
        return Err(AppError::BadRequest(
            "password must be at least 4 characters".into(),
        ));
    }
    let hash = auth::hash_password(&request.password)?;
    Ok(Json(
        db::users::create(&state.db, name, &hash, request.is_admin).await?,
    ))
}

/// Delete a user and their personal data (admin only; the owner is
/// protected).
#[utoipa::path(delete, path = "/users/{id}", tag = "users",
    params(("id" = i64, Path, description = "User id")),
    responses((status = 204), (status = 400), (status = 401), (status = 404)))]
pub async fn delete_user(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> AppResult<axum::http::StatusCode> {
    require_admin(&current)?;
    if id == 1 {
        return Err(AppError::BadRequest(
            "the owner account cannot be deleted".into(),
        ));
    }
    if db::users::delete(&state.db, id).await? {
        Ok(axum::http::StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound(format!("user {id}")))
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SetPasswordRequest {
    pub password: String,
}

/// Change a password: admins for anyone, users for themselves.
#[utoipa::path(put, path = "/users/{id}/password", tag = "users",
    params(("id" = i64, Path, description = "User id")),
    request_body = SetPasswordRequest,
    responses((status = 204), (status = 400), (status = 401), (status = 404)))]
pub async fn set_password(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentUser>,
    Path(id): Path<i64>,
    Json(request): Json<SetPasswordRequest>,
) -> AppResult<axum::http::StatusCode> {
    if !current.is_admin && current.id != id {
        return Err(AppError::BadRequest(
            "you can only change your own password".into(),
        ));
    }
    if request.password.chars().count() < 4 {
        return Err(AppError::BadRequest(
            "password must be at least 4 characters".into(),
        ));
    }
    let hash = auth::hash_password(&request.password)?;
    if db::users::set_password_hash(&state.db, id, &hash).await? {
        Ok(axum::http::StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound(format!("user {id}")))
    }
}

/// The authenticated identity of this request.
#[utoipa::path(get, path = "/auth/me", tag = "users",
    responses((status = 200, body = User), (status = 401)))]
pub async fn me(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentUser>,
) -> AppResult<Json<User>> {
    // The owner via API key has no stored row lookup requirement, but user 1
    // always exists (first migration), so report the database view of it.
    Ok(Json(
        db::users::get(&state.db, current.id)
            .await?
            .unwrap_or(User {
                id: current.id,
                name: current.name.clone(),
                is_admin: current.is_admin,
                has_password: false,
            }),
    ))
}

/// Invalidate the presented bearer token.
#[utoipa::path(post, path = "/auth/logout", tag = "users",
    responses((status = 204), (status = 401)))]
pub async fn logout(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> AppResult<axum::http::StatusCode> {
    if let Some(token) = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        db::users::delete_token(&state.db, token).await?;
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_users, create_user))
        .routes(routes!(delete_user))
        .routes(routes!(set_password))
        .routes(routes!(me))
        .routes(routes!(logout))
}
