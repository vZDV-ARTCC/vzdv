//! Structs and data to be shared across multiple parts of the site.

use crate::vatusa;
use axum::extract::rejection::FormRejection;
use axum::{
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::{NaiveDateTime, TimeZone, Utc};
use log::{error, info, warn};
use mini_moka::sync::Cache;
use minijinja::{Environment, context};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{Pool, Sqlite};
use std::{
    sync::Arc,
    sync::{LazyLock, OnceLock},
    time::Instant,
};
use tower_sessions_sqlx_store::sqlx::SqlitePool;
use vzdv::{
    GENERAL_HTTP_CLIENT, PermissionsGroup,
    config::Config,
    controller_can_see,
    email::{self, send_mail},
    sql::{self, Controller},
    vatusa::VatusaError,
};

/// Discord webhook for reporting errors.
///
/// Here as a global since the error handling functions don't
/// otherwise have access to the loaded config struct.
pub static ERROR_WEBHOOK: OnceLock<String> = OnceLock::new();

/// Error handling for all possible issues.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Session(#[from] tower_sessions::session::Error),
    #[error(transparent)]
    Templates(#[from] minijinja::Error),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    HttpCall(#[from] reqwest::Error),
    #[error("remote site query of {0} returned status {1}")]
    HttpResponse(&'static str, u16),
    #[error(transparent)]
    VatsimApi(#[from] vatsim_utils::errors::VatsimUtilError),
    #[error("error accessing VATUSA API: {0}")]
    VatusaApi(VatusaError),
    #[error(transparent)]
    ChronoParse(#[from] chrono::ParseError),
    #[error(transparent)]
    ChronoTimezone(#[from] chrono_tz::ParseError),
    #[error("other chrono error")]
    ChronoOther(&'static str),
    #[error(transparent)]
    NumberParsing(#[from] std::num::ParseIntError),
    #[error(transparent)]
    FormExtractionRejection(#[from] FormRejection),
    #[error("could not get value of form key")]
    MultipartFormGet,
    #[error(transparent)]
    MultipartFormParsing(#[from] axum::extract::multipart::MultipartError),
    #[error("error sending an email: {0}")]
    EmailError(anyhow::Error),
    #[error(transparent)]
    FileWriteError(#[from] std::io::Error),
    #[error(transparent)]
    JsonProcessingError(#[from] serde_json::Error),
    #[error("error removing controller from roster: {0}")]
    RosterRemovalError(&'static str),
    #[error("generic error {0}: {1}")]
    GenericFallback(&'static str, anyhow::Error),
}

impl AppError {
    fn friendly_message(&self) -> &'static str {
        match self {
            Self::Session(_) => "Issue accessing session data",
            Self::Templates(_) => "Issue generating page",
            Self::Database(_) => "Issue accessing database",
            Self::HttpCall(_) => "Issue sending HTTP call",
            Self::HttpResponse(_, _) => "Issue processing HTTP response",
            Self::VatsimApi(_) => "Issue accessing VATSIM API",
            Self::VatusaApi(_) => "Issue accessing VATUSA API",
            Self::ChronoParse(_) => "Issue processing time data",
            Self::ChronoTimezone(_) => "Issue processing timezone data",
            Self::ChronoOther(_) => "Issue processing time",
            Self::NumberParsing(_) => "Issue parsing numbers",
            Self::FormExtractionRejection(_) => "Issue getting info from you",
            Self::MultipartFormGet => "Issue parsing form key",
            Self::MultipartFormParsing(_) => "Issue parsing form submission",
            Self::EmailError(_) => "Issue sending an email",
            Self::FileWriteError(_) => "Writing to a file",
            Self::JsonProcessingError(_) => "error processing JSON",
            Self::RosterRemovalError(_) => "error removing controller from roster",
            Self::GenericFallback(_, _) => "Unknown error",
        }
    }
}

/// Try to construct the error page.
fn try_build_error_page(error: AppError) -> Result<String, AppError> {
    let mut env = Environment::new();
    env.add_template("_layout.jinja", include_str!("../templates/_layout.jinja"))?;
    env.add_template("_error.jinja", include_str!("../templates/_error.jinja"))?;
    let template = env.get_template("_error.jinja")?;
    let rendered = template.render(context! {
        error => error.friendly_message(),
        no_links => true,
    })?;
    Ok(rendered)
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let error_msg = format!("{self}");
        error!("Unhandled error: {error_msg}");
        let status = match &self {
            Self::FormExtractionRejection(e) => match e {
                FormRejection::FailedToDeserializeForm(_)
                | FormRejection::FailedToDeserializeFormBody(_) => StatusCode::BAD_REQUEST,
                FormRejection::InvalidFormContentType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            },
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        // report errors to Discord webhook
        tokio::spawn(async move {
            if let Some(url) = ERROR_WEBHOOK.get() {
                let res = GENERAL_HTTP_CLIENT
                    .post(url)
                    .json(&json!({
                        "content": format!("Error occurred, returning status {status}: {error_msg}")
                    }))
                    .send()
                    .await;
                if let Err(e) = res {
                    error!("Could not send error to Discord webhook: {e}");
                }
            }
        });

        // attempt to construct the error page, falling back to simple plain text if anything failed
        match try_build_error_page(self) {
            Ok(body) => (status, Html(body)).into_response(),
            Err(e) => {
                error!("Error building error page: {e}");
                (status, "Something went very wrong").into_response()
            }
        }
    }
}

/// Data wrapper for items in the server-side cache.
#[derive(Clone)]
pub struct CacheEntry {
    pub inserted: Instant,
    pub data: String,
}

impl CacheEntry {
    /// Wrap the data with a timestamp.
    pub fn new(data: String) -> Self {
        Self {
            inserted: Instant::now(),
            data,
        }
    }
}

/// App's state, available in all handlers via an extractor.
pub struct AppState {
    /// App config
    pub config: Config,
    /// Access to the DB
    pub db: SqlitePool,
    /// Loaded templates
    pub templates: Environment<'static>,
    /// Server-side cache for heavier-compute rendered templates
    pub cache: Cache<String, CacheEntry>,
}

/// Key for user info CRUD in session.
pub const SESSION_USER_INFO_KEY: &str = "USER_INFO";
/// Key for flashed messages CRUD in session.
pub const SESSION_FLASHED_MESSAGES_KEY: &str = "FLASHED_MESSAGES";

/// Data stored in the user's session.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UserInfo {
    pub cid: u32,
    pub first_name: String,
    pub last_name: String,

    pub is_home: bool,

    pub is_some_staff: bool,
    pub is_named_staff: bool,
    pub is_training_staff: bool,
    pub is_event_staff: bool,
    pub is_admin: bool,
}

/// Returns a response to redirect to the homepage for non-staff users.
///
/// This function checks the database to ensure that the staff member is
/// still actually a staff member at the time of making the request.
///
/// So long as the permissions being checked against aren't `PermissionsGroup::Anon`,
/// it's safe to assume that `user_info` is `Some<UserInfo>`.
pub async fn reject_if_not_in(
    state: &Arc<AppState>,
    user_info: &Option<UserInfo>,
    permissions: PermissionsGroup,
) -> Option<Redirect> {
    if is_user_member_of(state, user_info, permissions).await {
        None
    } else {
        info!(
            "Rejected access for {} to a resource",
            user_info.as_ref().map(|ui| ui.cid).unwrap_or_default()
        );
        Some(Redirect::to("/"))
    }
}

/// Return whether the user is a member of the corresponding staff group.
///
/// This function checks the database to ensure that the staff member is
/// still actually a staff member at the time of making the request.
///
/// So long as the permissions being checked against aren't `PermissionsGroup::Anon`,
/// it's safe to assume that `user_info` is `Some<UserInfo>`.
pub async fn is_user_member_of(
    state: &Arc<AppState>,
    user_info: &Option<UserInfo>,
    permissions: PermissionsGroup,
) -> bool {
    if user_info.is_none() {
        return false;
    }
    let user_info = user_info.as_ref().unwrap();
    let controller: Option<Controller> = match sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
        .bind(user_info.cid)
        .fetch_optional(&state.db)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            error!("Unknown controller with CID {}: {e}", user_info.cid);
            return false;
        }
    };
    controller_can_see(&controller, permissions)
}

/// Convert an HTML `datetime-local` input and JS timezone name to a UTC timestamp.
///
/// Kind of annoying.
pub fn js_timestamp_to_utc(timestamp: &str, timezone: &str) -> Result<NaiveDateTime, AppError> {
    let tz: chrono_tz::Tz = timezone.parse()?;
    let original = NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M")?;
    let converted = tz
        .from_local_datetime(&original)
        .single()
        .ok_or_else(|| AppError::ChronoOther("Error parsing HTML datetime"))?
        .naive_utc();
    Ok(converted)
}

/// Send message to the audit webhook.
pub fn post_audit(config: &Config, message: String) {
    let audit_webhook = config.discord.webhooks.audit.clone();
    tokio::spawn(async move {
        let res = GENERAL_HTTP_CLIENT
            .post(&audit_webhook)
            .json(&json!({ "content": message }))
            .send()
            .await;
        if let Err(e) = res {
            error!("Could not send info to audit webhook: {e}");
        }
    });
}

static TAG_REGEX_REPLACEMENTS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?i)<form").unwrap(),
        Regex::new(r"(?i)<script").unwrap(),
        Regex::new(r"(?i)<button").unwrap(),
        Regex::new(r"(?i)<a").unwrap(),
    ]
});

/// Strip some tags from the HTML string for (relatively) safe direct rendering in the DOM.
///
/// I'm not really worried about the resulting string _looking_ okay, I just don't want
/// to render forms or scripts in people's browsers.
pub fn strip_some_tags(s: &str) -> String {
    let mut ret = s.to_string();
    for re in TAG_REGEX_REPLACEMENTS.iter() {
        ret = re.replace_all(&ret, "").to_string();
    }
    ret
}

/// Add an audit log message to the DB.
///
/// If `log` is true, then the message is also logged via `log::info!`.
pub async fn record_log(message: String, db: &Pool<Sqlite>, log: bool) -> Result<(), AppError> {
    sqlx::query(sql::CREATE_LOG)
        .bind(&message)
        .bind(Utc::now())
        .execute(db)
        .await?;
    if log {
        info!("{message}");
    }
    Ok(())
}

/// Remove a controller from the facility roster, either home or visiting.
///
/// This method updates VATUSA and the DB, but communicating success/failure
/// to the user initiating this action is the calling code's responsibility.
pub async fn remove_controller_from_roster(
    cid: u32,
    remover: u32,
    reason: &str,
    db: &Pool<Sqlite>,
    config: &Config,
) -> Result<(), AppError> {
    let controller: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
        .bind(cid)
        .fetch_optional(db)
        .await?;
    let controller = match controller {
        Some(c) => c,
        None => {
            warn!("{remover} tried to remove unknown controller {cid} from roster");
            return Err(AppError::RosterRemovalError("unknown controller"));
        }
    };
    if !controller.is_on_roster {
        warn!("{remover} tried to remove off-roster controller {cid}");
        return Err(AppError::RosterRemovalError("off-roster controller"));
    }
    let home_controller = controller.home_facility == "ZDV";
    // update VATUSA
    if home_controller {
        vatusa::remove_home_controller(
            cid,
            &remover.to_string(),
            reason,
            &config.vatsim.vatusa_api_key,
        )
        .await?;
    } else {
        vatusa::remove_visiting_controller(cid, reason, &config.vatsim.vatusa_api_key)
            .await
            .map_err(|_| AppError::RosterRemovalError("error with VATUSA"))?;
        // emails must be sent for removing visiting controllers, as VATUSA does not notify
        let controller_info =
            vatusa::get_controller_info(cid, Some(&config.vatsim.vatusa_api_key)).await?;
        if let Some(ref email) = controller_info.email {
            send_mail(
                config,
                db,
                &format!(
                    "{} {}",
                    controller_info.first_name, controller_info.last_name
                ),
                email,
                email::templates::VISITOR_REMOVED,
                None,
            )
            .await
            .map_err(AppError::EmailError)?;
        } else {
            warn!("Could not send visitor removal email for {cid} due to lack of email address");
        }
    }

    // update DB
    sqlx::query(sql::UPDATE_REMOVED_FROM_ROSTER)
        .bind(cid)
        .execute(db)
        .await?;
    record_log(
        format!(
            "{remover} removed {cid} from {} roster: {reason}",
            if home_controller { "home" } else { "visiting " }
        ),
        db,
        true,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::strip_some_tags;

    #[test]
    fn test_strip_some_tags() {
        assert_eq!(
            strip_some_tags(r#"foo <script src="https://example.com"></script> bar"#),
            r#"foo  src="https://example.com"></script> bar"#
        );
        assert_eq!(
            strip_some_tags(r#"foo <SCRIPT src="https://example.com"></SCRIPT> bar"#),
            r#"foo  src="https://example.com"></SCRIPT> bar"#
        );
        assert_eq!(
            strip_some_tags(
                r#"foo <fORm method="POST" action="https://example.com"></SCRIPT> bar"#
            ),
            r#"foo  method="POST" action="https://example.com"></SCRIPT> bar"#
        );
        assert_eq!(
            strip_some_tags(r#"something <button type="submit"></button>"#),
            r#"something  type="submit"></button>"#
        );
        assert_eq!(
            strip_some_tags(r#"click <a href="https://example.com">here</a> to win"#),
            r#"click  href="https://example.com">here</a> to win"#
        );
    }
}
