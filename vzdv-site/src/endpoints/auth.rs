//! HTTP endpoints for logging in and out.

use crate::shared::{AppError, AppState, SESSION_USER_INFO_KEY, UserInfo, record_log};
use axum::{
    Router,
    extract::{Query, State},
    response::{Html, Redirect},
    routing::get,
};
use log::debug;
use minijinja::context;
use std::sync::Arc;
use tower_sessions::Session;
use vzdv::{
    controller_can_see,
    sql::{self, Controller},
    vatsim::{AuthCallback, code_to_tokens, get_user_info, oauth_redirect_start},
};

/// Login page.
///
/// Doesn't actually have a template to render; the user is immediately redirected to
/// either the homepage if they're already logged in, or the VATSIM OAuth page to start
/// their login flow.
async fn page_auth_login(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Redirect, AppError> {
    // if already logged in, just redirect to homepage
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(user_info) = user_info {
        debug!("Already logged-in user {} hit login page", user_info.cid);
        return Ok(Redirect::to("/"));
    }
    let redirect_url = oauth_redirect_start(&state.config);
    Ok(Redirect::to(&redirect_url))
}

/// Auth callback.
///
/// The user is redirected here from VATSIM OAuth providing, in
/// the URL, a code to use in getting an access token for them.
async fn page_auth_callback(
    query: Query<AuthCallback>,
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    let token_data = code_to_tokens(&query.code, &state.config)
        .await
        .map_err(|err| AppError::GenericFallback("getting auth token from code", err))?;
    let session_user_info = get_user_info(&token_data.access_token, &state.config)
        .await
        .map_err(|err| AppError::GenericFallback("getting auth user info", err))?;
    let db_user_info: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
        .bind(&session_user_info.data.cid)
        .fetch_optional(&state.db)
        .await?;
    let roles: Vec<_> = match &db_user_info {
        Some(controller) => controller.roles.split_terminator(',').collect::<Vec<_>>(),
        None => Vec::new(),
    };

    let to_session = UserInfo {
        cid: session_user_info.data.cid.parse()?,
        first_name: session_user_info.data.personal.name_first,
        last_name: session_user_info.data.personal.name_last,

        is_home: db_user_info
            .as_ref()
            .map(|c| c.home_facility == "ZDV")
            .unwrap_or_default(),

        on_roster: db_user_info.as_ref().is_some_and(|c| c.is_on_roster),
        is_some_staff: !roles.is_empty(),
        is_named_staff: controller_can_see(&db_user_info, vzdv::PermissionsGroup::NamedPosition),
        is_training_staff: controller_can_see(&db_user_info, vzdv::PermissionsGroup::TrainingTeam),
        is_event_staff: controller_can_see(&db_user_info, vzdv::PermissionsGroup::EventsTeam),
        is_admin: controller_can_see(&db_user_info, vzdv::PermissionsGroup::Admin),
    };
    session
        .insert(SESSION_USER_INFO_KEY, to_session.clone())
        .await?;
    sqlx::query(sql::UPSERT_USER_LOGIN)
        .bind(to_session.cid)
        .bind(&to_session.first_name)
        .bind(&to_session.last_name)
        .bind(&session_user_info.data.personal.email)
        .bind(session_user_info.data.vatsim.rating.id)
        .execute(&state.db)
        .await?;

    record_log(
        format!("Completed log in for {}", session_user_info.data.cid),
        &state.db,
        true,
    )
    .await?;
    let template = state.templates.get_template("auth/login_complete.jinja")?;
    let rendered = template.render(context! { user_info => to_session })?;
    Ok(Html(rendered))
}

/// Clear session and redirect to homepage.
async fn page_auth_logout(session: Session) -> Result<Redirect, AppError> {
    // don't need to check if there's something here
    session.delete().await?;
    Ok(Redirect::to("/"))
}

/// This file's routes and templates.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/auth/log_in", get(page_auth_login))
        .route("/auth/logout", get(page_auth_logout))
        .route("/auth/callback", get(page_auth_callback))
}
