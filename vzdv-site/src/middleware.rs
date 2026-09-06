//! App middleware functions.

use crate::shared::{AppError, AppState, SESSION_USER_INFO_KEY, UserInfo, refresh_roster_status};
use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use chrono::Utc;
use log::{debug, error, warn};
use std::{
    collections::HashSet,
    sync::{Arc, LazyLock},
};
use tower_sessions::{Expiry, Session};
use vzdv::sql::{self, Resource};

static IGNORE_PATHS: LazyLock<HashSet<&str>> = LazyLock::new(|| HashSet::from(["/favicon.ico"]));

/// Cookie expiration duration (hours).
pub const SESSION_INACTIVITY_WINDOW: i64 = 24;

/// Simple logging middleware.
///
/// Logs the method, path, and response code to debug
/// if processing returned a successful code, and to
/// warn otherwise.
pub async fn logging(request: Request, next: Next) -> Response {
    let uri = request.uri().clone();
    let path = uri.path();
    if !IGNORE_PATHS.contains(path) {
        let method = request.method().clone();
        let response = next.run(request).await;
        let s = format!("{} {} {}", method, path, response.status().as_u16());
        if response.status().is_success() || response.status().is_redirection() {
            debug!("{s}");
        } else {
            warn!("{s}");
        }
        response
    } else {
        next.run(request).await
    }
}

/// Middleware to record a logged-in user accessing an asset static endpoint.
///
/// This is used to asset that a user has actually opened a resource before they're
/// allowed to initial on SOPs. This middleware updates the datbase (in a Tokio task)
/// with the date of the user opening the resource if it's an SOP document. This
/// middleware doesn't do anything for non-SOP resources.
pub async fn asset_access(
    State(state): State<Arc<AppState>>,
    session: Session,
    request: Request,
    next: Next,
) -> Response {
    let path = {
        // just Rust doing Rust things ...
        let uri = request.uri().clone();
        let path = uri.path();
        let path = &path[8..path.len()];
        path.to_string()
    };
    if let Ok(Some(user_info)) = session.get::<UserInfo>(SESSION_USER_INFO_KEY).await {
        tokio::spawn(async move {
            let resource: Result<Option<Resource>, sqlx::Error> =
                sqlx::query_as(sql::GET_RESOURCE_BY_FILE_NAME)
                    .bind(&path)
                    .fetch_optional(&state.db)
                    .await;
            match resource {
                Ok(Some(resource)) if resource.category == "SOPs" => {
                    let upsert_res = sqlx::query(sql::UPSERT_SOP_ACCESS)
                        .bind(user_info.cid)
                        .bind(resource.id)
                        .bind(Utc::now())
                        .execute(&state.db)
                        .await;
                    if let Err(e) = upsert_res {
                        error!(
                            "Error updating SOP access time for {} {}: {e}",
                            user_info.cid, resource.id
                        );
                    }
                }
                Err(e) => {
                    error!("Error in SOP access middleware getting resources from the DB: {e}")
                }
                _ => {}
            }
        });
    }
    next.run(request).await
}

/// Middleware to extend the `tower_sessions` session cookie on the user's browser.
///
/// The cookie's initial duration covers how long the cookie will last between
/// site visits, as this middleware extends the duration of the cookie by the
/// same amount each time the user visits any page on the site.
///
/// This does touch the DB, which I don't love.
pub async fn extend_session(
    State(state): State<Arc<AppState>>,
    session: Session,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    session.set_expiry(Some(Expiry::OnInactivity(time::Duration::hours(
        SESSION_INACTIVITY_WINDOW,
    ))));
    refresh_roster_status(&state.db, &session).await?;
    Ok(next.run(request).await)
}
