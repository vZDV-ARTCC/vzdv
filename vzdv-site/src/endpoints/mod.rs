//! HTTP endpoints.

use crate::{
    flashed_messages,
    shared::{AppError, AppState, SESSION_USER_INFO_KEY, UserInfo, record_log},
};
use axum::{
    Form, Router,
    extract::State,
    response::{Html, Redirect},
    routing::{get, post},
};
use log::error;
use minijinja::context;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tower_http::services::ServeDir;
use tower_sessions::Session;
use vzdv::{
    GENERAL_HTTP_CLIENT,
    sql::{self, Controller},
};

pub mod admin;
pub mod airspace;
pub mod api;
pub mod auth;
pub mod controller;
pub mod events;
pub mod facility;
pub mod homepage;
pub mod ids;
pub mod user;

/// 404 not found page.
///
/// Redirected to whenever the router cannot find a valid handler for the requested path.
pub async fn page_404(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    let template = state.templates.get_template("404.jinja")?;
    let rendered = template.render(context! { no_links => true })?;
    Ok(Html(rendered))
}

/// View the feedback form.
///
/// The template handles requiring the user to be logged in.
async fn page_feedback_form(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let mut all_controllers: Vec<Controller> = sqlx::query_as(sql::GET_ALL_CONTROLLERS_ON_ROSTER)
        .fetch_all(&state.db)
        .await?;
    all_controllers.sort_by_key(|c| format!("{} {}", c.first_name, c.last_name));
    let all_controllers: Vec<(u32, String)> = all_controllers
        .iter()
        .map(|controller| {
            (
                controller.cid,
                format!("{} {}", controller.first_name, controller.last_name,),
            )
        })
        .collect();
    let template = state.templates.get_template("feedback.jinja")?;
    let rendered = template.render(context! { user_info, flashed_messages, all_controllers })?;
    Ok(Html(rendered))
}

#[derive(Debug, Deserialize)]
struct FeedbackForm {
    controller: u32,
    position: String,
    rating: String,
    comments: String,
    email: String,
    contact_me: Option<String>,
}

/// Submit the feedback form.
async fn page_feedback_form_post(
    State(state): State<Arc<AppState>>,
    session: Session,
    Form(feedback): Form<FeedbackForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(user_info) = user_info {
        sqlx::query(sql::INSERT_FEEDBACK)
            .bind(feedback.controller)
            .bind(&feedback.position)
            .bind(&feedback.rating)
            .bind(&feedback.comments)
            .bind(sqlx::types::chrono::Utc::now())
            .bind(user_info.cid)
            .bind(feedback.contact_me.is_some())
            .bind(&feedback.email)
            .execute(&state.db)
            .await?;
        flashed_messages::push_flashed_message(
            session,
            flashed_messages::MessageLevel::Success,
            "Feedback submitted, thank you!",
        )
        .await?;
        record_log(
            format!(
                "{} submitted feedback for {}",
                user_info.cid, feedback.controller
            ),
            &state.db,
            true,
        )
        .await?;
        let notification_webhook = state.config.discord.webhooks.new_feedback.clone();
        let for_controller: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
            .bind(feedback.controller)
            .fetch_optional(&state.db)
            .await?;
        let message = match for_controller {
            Some(c) => {
                format!(
                    "New {} feedback posted for {} {} ({}) on **{}** by {}:\n\t{}",
                    feedback.rating,
                    c.first_name,
                    c.last_name,
                    c.operating_initials.unwrap_or_else(|| String::from("??")),
                    feedback.position,
                    user_info.cid,
                    if feedback.comments.len() >= 1_500 {
                        format!("{} ...", &feedback.comments[0..1_500])
                    } else {
                        feedback.comments
                    }
                )
            }
            None => {
                format!(
                    "New {} feedback posted for an unknown controller on **{}** by {}:\n\t{}",
                    feedback.rating,
                    feedback.position,
                    user_info.cid,
                    if feedback.comments.len() >= 1_500 {
                        format!("{} ...", &feedback.comments[0..1_500])
                    } else {
                        feedback.comments
                    }
                )
            }
        };
        tokio::spawn(async move {
            let res = GENERAL_HTTP_CLIENT
                .post(&notification_webhook)
                .json(&json!({ "content": message }))
                .send()
                .await;
            if let Err(e) = res {
                error!("Could not send info to new feedback webhook: {e}");
            }
        });
    } else {
        flashed_messages::push_flashed_message(
            session,
            flashed_messages::MessageLevel::Error,
            "You must be logged in to submit feedback.",
        )
        .await?;
    }
    Ok(Redirect::to("/feedback"))
}

/// Changelog.
///
/// Manually updated.
async fn page_changelog(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let template = state.templates.get_template("changelog.jinja")?;
    let rendered = template.render(context! { user_info })?;
    Ok(Html(rendered))
}

/// Privacy policy.
async fn page_privacy_policy(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let template = state.templates.get_template("privacy_policy.jinja")?;
    let rendered = template.render(context! { user_info })?;
    Ok(Html(rendered))
}

/// Privacy policy.
async fn page_terms_of_use(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let template = state.templates.get_template("terms_of_use.jinja")?;
    let rendered = template.render(context! { user_info })?;
    Ok(Html(rendered))
}

/// This file's routes and templates.
pub fn router(app_state: &Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/404", get(page_404))
        .route("/feedback", get(page_feedback_form))
        .route("/feedback", post(page_feedback_form_post))
        .route("/changelog", get(page_changelog))
        .route("/privacy_policy", get(page_privacy_policy))
        .route("/terms_of_use", get(page_terms_of_use))
        .nest_service("/assets", ServeDir::new("assets"))
        .route_layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            crate::middleware::asset_access,
        ))
        .nest_service("/static", ServeDir::new("static"))
}
