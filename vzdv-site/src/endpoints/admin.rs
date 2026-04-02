//! Endpoints for editing and controlling aspects of the site.

use crate::{
    flashed_messages::{self, MessageLevel, push_flashed_message},
    shared::{
        AppError, AppState, CacheEntry, SESSION_USER_INFO_KEY, UserInfo, is_user_member_of,
        post_audit, record_log, reject_if_not_in, remove_controller_from_roster,
    },
    vatusa::{self, add_visiting_controller},
};
use axum::{
    Form, Router,
    extract::{DefaultBodyLimit, Json, Multipart, Path, State},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{delete, get, post},
};
use axum_extra::extract::Query;
use chrono::{DateTime, Utc};
use itertools::Itertools;
use log::{debug, error, info, warn};
use minijinja::context;
use reqwest::StatusCode;
use rev_buf_reader::RevBufReader;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use std::{collections::HashMap, io::BufRead, path::Path as FilePath, sync::Arc, time::Instant};
use tower_sessions::Session;
use uuid::Uuid;
use vzdv::{
    ControllerRating, GENERAL_HTTP_CLIENT, PermissionsGroup,
    email::{self, send_mail},
    generate_operating_initials_for, get_controller_cids_and_names, retrieve_all_in_use_ois,
    sql::{
        self, Activity, Controller, Feedback, FeedbackForReview, Log, NoShow, Resource, SoloCert,
        SopInitial, VisitorRequest,
    },
    vatusa::{get_multiple_controller_info, get_multiple_controller_names},
};

/// Page for managing controller feedback.
///
/// Feedback must be reviewed by staff before being posted to Discord.
///
/// Admin staff members only.
async fn page_feedback(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect.into_response());
    }
    let pending_feedback: Vec<FeedbackForReview> =
        sqlx::query_as(sql::GET_PENDING_FEEDBACK_FOR_REVIEW)
            .fetch_all(&state.db)
            .await?;
    let cid_names = get_multiple_controller_names(
        &pending_feedback
            .iter()
            .filter(|pf| pf.reviewer_action == "pending")
            .map(|pf| pf.submitter_cid)
            .collect::<Vec<_>>(),
    )
    .await;
    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let template = state.templates.get_template("admin/feedback.jinja")?;
    let rendered = template.render(context! {
        user_info,
        flashed_messages,
        pending_feedback,
        cid_names,
    })?;
    Ok(Html(rendered).into_response())
}

#[derive(Debug, Deserialize)]
struct FeedbackReviewForm {
    id: u32,
    action: String,
}

/// Handler for staff members taking action on feedback.
///
/// Admin staff members only.
async fn post_feedback_form_handle(
    State(state): State<Arc<AppState>>,
    session: Session,
    Form(feedback_form): Form<FeedbackReviewForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect);
    }
    let user_info = user_info.unwrap();
    let db_feedback: Option<Feedback> = sqlx::query_as(sql::GET_FEEDBACK_BY_ID)
        .bind(feedback_form.id)
        .fetch_optional(&state.db)
        .await?;
    if let Some(feedback) = db_feedback {
        if feedback_form.action == "Archive" {
            sqlx::query(sql::UPDATE_FEEDBACK_TAKE_ACTION)
                .bind(user_info.cid)
                .bind("archive")
                .bind(false)
                .bind(feedback_form.id)
                .execute(&state.db)
                .await?;
            record_log(
                format!("{} archived feedback {}", user_info.cid, feedback.id),
                &state.db,
                true,
            )
            .await?;
            flashed_messages::push_flashed_message(
                session,
                MessageLevel::Success,
                "Feedback archived",
            )
            .await?;
        } else if feedback_form.action == "Delete" {
            sqlx::query(sql::DELETE_FROM_FEEDBACK)
                .bind(feedback_form.id)
                .execute(&state.db)
                .await?;
            record_log(
                format!(
                    "{} deleted {} feedback {} for {} by {}",
                    user_info.cid,
                    feedback.rating,
                    feedback.id,
                    feedback.controller,
                    feedback.submitter_cid
                ),
                &state.db,
                true,
            )
            .await?;
            flashed_messages::push_flashed_message(
                session,
                MessageLevel::Success,
                "Feedback deleted",
            )
            .await?;
        } else if feedback_form.action == "Post to Discord" {
            let controller: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
                .bind(feedback.controller)
                .fetch_optional(&state.db)
                .await?;
            GENERAL_HTTP_CLIENT
                .post(&state.config.discord.webhooks.feedback)
                .json(&json!({
                    "content": "",
                    "embeds": [{
                        "title": "Feedback received",
                        "fields": [
                            {
                                "name": "Controller",
                                "value": controller.map(|c| format!("{} {}", c.first_name, c.last_name)).unwrap_or_default()
                            },
                            {
                                "name": "Position",
                                "value": feedback.position
                            },
                            {
                                "name": "Rating",
                                "value": match feedback.rating.as_str() {
                                    "excellent" => "Excellent",
                                    "good" => "Good",
                                    "fair" => "Fair",
                                    "poor" => "Poor",
                                    _ => "?"
                                }
                            },
                            {
                                "name": "Comments",
                                "value": feedback.comments
                            }
                        ]
                    }]
                }))
                .send()
                .await?;
            record_log(
                format!(
                    "{} submitted feedback {} to Discord",
                    user_info.cid, feedback.id
                ),
                &state.db,
                true,
            )
            .await?;
            sqlx::query(sql::UPDATE_FEEDBACK_TAKE_ACTION)
                .bind(user_info.cid)
                .bind("post")
                .bind(true)
                .bind(feedback_form.id)
                .execute(&state.db)
                .await?;
            flashed_messages::push_flashed_message(
                session,
                MessageLevel::Success,
                "Feedback shared",
            )
            .await?;
        } else if feedback_form.action == "Silently approve" {
            sqlx::query(sql::UPDATE_FEEDBACK_TAKE_ACTION)
                .bind(user_info.cid)
                .bind("approve")
                .bind(false)
                .bind(feedback_form.id)
                .execute(&state.db)
                .await?;
            record_log(
                format!(
                    "{} silently-approved {} feedback {} for {} by {}",
                    user_info.cid,
                    feedback.rating,
                    feedback.id,
                    feedback.controller,
                    feedback.submitter_cid
                ),
                &state.db,
                true,
            )
            .await?;
            flashed_messages::push_flashed_message(
                session,
                MessageLevel::Success,
                "Feedback silently approved",
            )
            .await?;
        }
    } else {
        flashed_messages::push_flashed_message(session, MessageLevel::Error, "Feedback not found")
            .await?;
    }

    Ok(Redirect::to("/admin/feedback"))
}

#[derive(Debug, Deserialize)]
struct FeedbackEditForm {
    id: u32,
    comments: String,
}

async fn post_feedback_edited_form_handle(
    State(state): State<Arc<AppState>>,
    session: Session,
    Form(edit_form): Form<FeedbackEditForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect);
    }
    let user_info = user_info.unwrap();
    let db_feedback: Option<Feedback> = sqlx::query_as(sql::GET_FEEDBACK_BY_ID)
        .bind(edit_form.id)
        .fetch_optional(&state.db)
        .await?;
    if db_feedback.is_some() {
        sqlx::query(sql::UPDATE_FEEDBACK_COMMENTS)
            .bind(edit_form.id)
            .bind(&edit_form.comments)
            .execute(&state.db)
            .await?;
        flashed_messages::push_flashed_message(
            session,
            MessageLevel::Info,
            "Feedback comments updated",
        )
        .await?;
        record_log(
            format!(
                "{} updated feedback {} comments",
                user_info.cid, edit_form.id
            ),
            &state.db,
            true,
        )
        .await?;
    } else {
        flashed_messages::push_flashed_message(session, MessageLevel::Error, "Unknown feedback ID")
            .await?;
        warn!(
            "{} tried to edit unknown feedback {}",
            user_info.cid, edit_form.id
        );
    }
    Ok(Redirect::to("/admin/feedback"))
}

/// Page to set email templates and send emails.
///
/// Admin staff members only.
async fn page_emails(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect.into_response());
    }
    let all_controllers: Vec<Controller> = sqlx::query_as(sql::GET_ALL_CONTROLLERS)
        .fetch_all(&state.db)
        .await?;
    let template = state.templates.get_template("admin/emails.jinja")?;
    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let email_templates = email::query_templates(&state.db)
        .await
        .map_err(AppError::EmailError)?;
    let rendered = template.render(context! {
        user_info,
        all_controllers,
        flashed_messages,
        visitor_accepted => email_templates.visitor_accepted,
        visitor_denied => email_templates.visitor_denied,
        visitor_removed => email_templates.visitor_removed,
        currency_required => email_templates.currency_required,
    })?;
    Ok(Html(rendered).into_response())
}

#[derive(Debug, Deserialize)]
struct UpdateTemplateForm {
    name: String,
    subject: String,
    body: String,
}

/// Form submission to update an email template.
///
/// Admin staff members only.
async fn post_email_template_update(
    State(state): State<Arc<AppState>>,
    session: Session,
    Form(update_template_form): Form<UpdateTemplateForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect);
    }
    // verify it's one of the templates
    if ![
        email::templates::VISITOR_ACCEPTED,
        email::templates::VISITOR_DENIED,
        email::templates::VISITOR_REMOVED,
        email::templates::CURRENCY_REQUIRED,
    ]
    .iter()
    .any(|&name| name == update_template_form.name)
    {
        flashed_messages::push_flashed_message(
            session,
            MessageLevel::Error,
            &format!("Unknown template name: {}", update_template_form.name),
        )
        .await?;
        return Ok(Redirect::to("/admin/emails"));
    }
    // save
    sqlx::query(sql::UPDATE_EMAIL_TEMPLATE)
        .bind(update_template_form.name)
        .bind(update_template_form.subject)
        .bind(update_template_form.body)
        .execute(&state.db)
        .await?;
    flashed_messages::push_flashed_message(session, MessageLevel::Info, "Email template updated")
        .await?;
    Ok(Redirect::to("/admin/emails"))
}

#[derive(Debug, Deserialize)]
struct ManualEmailForm {
    recipient: u32,
    template: String,
}

/// Form submission to manually send an email.
///
/// Admin staff members only.
async fn post_email_manual_send(
    State(state): State<Arc<AppState>>,
    session: Session,
    Form(manual_email_form): Form<ManualEmailForm>,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect.into_response());
    }
    let controller: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
        .bind(manual_email_form.recipient)
        .fetch_optional(&state.db)
        .await?;
    let controller = match controller {
        Some(c) => c,
        None => {
            flashed_messages::push_flashed_message(
                session,
                MessageLevel::Error,
                "Unknown controller",
            )
            .await?;
            return Ok(Redirect::to("/admin/emails").into_response());
        }
    };
    let controller_info = vatusa::get_controller_info(
        manual_email_form.recipient,
        Some(&state.config.vatsim.vatusa_api_key),
    )
    .await?;
    let email = match controller_info.email {
        Some(e) => e,
        None => {
            flashed_messages::push_flashed_message(
                session,
                MessageLevel::Error,
                "Could not get controller's email from VATUSA",
            )
            .await?;
            return Ok(Redirect::to("/admin/emails").into_response());
        }
    };
    record_log(
        format!(
            "{} sent {} email to {}",
            user_info.unwrap().cid,
            manual_email_form.template,
            manual_email_form.recipient
        ),
        &state.db,
        true,
    )
    .await?;
    send_mail(
        &state.config,
        &state.db,
        &format!("{} {}", controller.first_name, controller.last_name),
        &email,
        &manual_email_form.template,
        None,
    )
    .await
    .map_err(AppError::EmailError)?;
    flashed_messages::push_flashed_message(session, MessageLevel::Info, "Email sent").await?;
    Ok(Redirect::to("/admin/emails").into_response())
}

/// Page for logs.
///
/// Read the last hundred lines from each of the log files
/// and show them in the page.
///
/// Admin staff members only.
async fn page_logs(
    State(state): State<Arc<AppState>>,
    session: Session,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect.into_response());
    }
    let line_count: u64 = match params.get("lines") {
        Some(n) => match n.parse() {
            Ok(n) => n,
            Err(_) => {
                warn!("Error parsing 'lines' query param on logs page");
                100
            }
        },
        None => 100,
    };

    let file_names = ["vzdv_site.log", "vzdv_tasks.log", "vzdv_bot.log"];
    let mut logs: HashMap<&str, String> = HashMap::new();
    for name in file_names {
        let mut buffer = Vec::new();
        let file = match std::fs::File::open(name) {
            Ok(f) => f,
            Err(e) => {
                error!("Error reading log file: {e}");
                logs.insert(name, String::new());
                continue;
            }
        };
        let reader = RevBufReader::new(file);
        let mut by_line = reader.lines();
        for _ in 0..line_count {
            if let Some(line) = by_line.next() {
                let line = line.unwrap();
                buffer.push(line);
            } else {
                break;
            }
        }
        buffer.reverse();
        logs.insert(name, buffer.join("<br>"));
    }

    let template = state.templates.get_template("admin/logs.jinja")?;
    let rendered = template.render(context! { user_info, logs, line_count })?;
    Ok(Html(rendered).into_response())
}

/// Page for managing visitor applications.
///
/// Admin staff members only.
async fn page_visitor_applications(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect.into_response());
    }
    let requests: Vec<VisitorRequest> = sqlx::query_as(sql::GET_ALL_VISITOR_REQUESTS)
        .fetch_all(&state.db)
        .await?;
    let request_cids: Vec<_> = requests.iter().map(|request| request.cid).collect();
    let controller_info = get_multiple_controller_info(&request_cids).await;
    let already_visiting = request_cids.iter().fold(HashMap::new(), |mut map, cid| {
        let info = controller_info.iter().find(|&info| info.cid == *cid);
        if let Some(info) = info {
            let already_visiting: Vec<String> = info
                .visiting_facilities
                .as_ref()
                .map(|visiting| {
                    visiting
                        .iter()
                        .map(|visit| visit.facility.to_owned())
                        .collect()
                })
                .unwrap_or_default();
            map.insert(cid, already_visiting.join(", "));
        }
        map
    });

    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let template = state
        .templates
        .get_template("admin/visitor_applications.jinja")?;
    let rendered = template.render(context! {
        user_info,
        flashed_messages,
        requests,
        already_visiting,
    })?;
    Ok(Html(rendered).into_response())
}

#[derive(Deserialize)]
struct VisitorApplicationActionForm {
    action: String,
}

/// Form submission for managing visitor applications.
///
/// Admin staff members only.
async fn post_visitor_application_action(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(id): Path<u32>,
    Form(action_form): Form<VisitorApplicationActionForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect);
    }
    let user_info = user_info.unwrap();
    let request: Option<VisitorRequest> = sqlx::query_as(sql::GET_VISITOR_REQUEST_BY_ID)
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    let request = match request {
        Some(r) => r,
        None => {
            flashed_messages::push_flashed_message(
                session,
                MessageLevel::Error,
                "Visitor application not found",
            )
            .await?;
            return Ok(Redirect::to("/admin/visitor_applications"));
        }
    };
    let controller_info =
        vatusa::get_controller_info(request.cid, Some(&state.config.vatsim.vatusa_api_key)).await?;
    record_log(
        format!(
            "{} taking action {} on visitor request {id} for {} {} ({})",
            user_info.cid,
            action_form.action,
            controller_info.first_name,
            controller_info.last_name,
            request.cid
        ),
        &state.db,
        true,
    )
    .await?;

    if action_form.action == "accept" {
        // add to roster in VATUSA
        add_visiting_controller(request.cid, &state.config.vatsim.vatusa_api_key).await?;

        // update controller record now rather than waiting for the task sync
        sqlx::query(sql::SET_CONTROLLER_ON_ROSTER)
            .bind(request.cid)
            .bind(true)
            .execute(&state.db)
            .await?;

        // generate OIs
        let in_use = retrieve_all_in_use_ois(&state.db)
            .await
            .map_err(|e| AppError::GenericFallback("could not get OIs from DB", e))?;
        let new_ois = generate_operating_initials_for(
            &in_use,
            &controller_info.first_name,
            &controller_info.last_name,
        )
        .map_err(|e| AppError::GenericFallback("could not create new OIs", e))?;
        sqlx::query(sql::UPDATE_CONTROLLER_OIS)
            .bind(request.cid)
            .bind(&new_ois)
            .execute(&state.db)
            .await?;
        info!("New visitor {} given OIs {}", request.cid, new_ois);

        // inform if possible
        if let Some(email_address) = controller_info.email {
            send_mail(
                &state.config,
                &state.db,
                &format!("{} {}", request.first_name, request.last_name),
                &email_address,
                email::templates::VISITOR_ACCEPTED,
                None,
            )
            .await
            .map_err(AppError::EmailError)?;
            flashed_messages::push_flashed_message(
                session,
                MessageLevel::Success,
                "Visitor request accepted and the controller was emailed of the decision.",
            )
            .await?;
        } else {
            warn!("No email address found for {}", request.cid);
            flashed_messages::push_flashed_message(
                session,
                MessageLevel::Success,
                "Visitor request accepted, but their email could not be determined so no email was sent.",
            )
            .await?;
        }
    } else if action_form.action == "deny" {
        // inform if possible
        if let Some(email_address) = controller_info.email {
            send_mail(
                &state.config,
                &state.db,
                &format!("{} {}", request.first_name, request.last_name),
                &email_address,
                email::templates::VISITOR_DENIED,
                None,
            )
            .await
            .map_err(AppError::EmailError)?;
            flashed_messages::push_flashed_message(
                session,
                MessageLevel::Success,
                "Visitor request denied and the controller was emailed of the decision.",
            )
            .await?;
        } else {
            warn!("No email address found for {}", request.cid);
            flashed_messages::push_flashed_message(
                session,
                MessageLevel::Success,
                "Visitor request denied, but their email could not be determined so no email was sent.",
            )
            .await?;
        }
    }

    // delete the request
    sqlx::query(sql::DELETE_VISITOR_REQUEST)
        .bind(id)
        .execute(&state.db)
        .await?;

    Ok(Redirect::to("/admin/visitor_applications"))
}

/// Page for managing the site's resource documents and links.
///
/// Named staff members only.
async fn page_resources(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) =
        reject_if_not_in(&state, &user_info, PermissionsGroup::NamedPosition).await
    {
        return Ok(redirect.into_response());
    }
    let resources: Vec<Resource> = sqlx::query_as(sql::GET_ALL_RESOURCES)
        .fetch_all(&state.db)
        .await?;
    let categories = &state.config.database.resource_category_ordering;
    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let template = state.templates.get_template("admin/resources.jinja")?;
    let rendered =
        template.render(context! { user_info, flashed_messages, resources, categories })?;
    Ok(Html(rendered).into_response())
}

/// API endpoint for deleting a resource.
///
/// Named staff members only.
async fn api_delete_resource(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(id): Path<u32>,
) -> Result<StatusCode, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if !is_user_member_of(&state, &user_info, PermissionsGroup::NamedPosition).await {
        return Ok(StatusCode::FORBIDDEN);
    }
    let user_info = user_info.unwrap();
    let resource: Option<Resource> = sqlx::query_as(sql::GET_RESOURCE_BY_ID)
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    let resource = match resource {
        Some(r) => r,
        None => {
            warn!("{} tried to delete unknown resource {id}", user_info.cid);
            return Ok(StatusCode::NOT_FOUND);
        }
    };
    // delete all access records for this SOP
    sqlx::query(sql::DELETE_SOP_ACCESS_FOR_RESOURCE)
        .bind(id)
        .execute(&state.db)
        .await?;
    // delete all initials for this SOP
    sqlx::query(sql::DELETE_SOP_INITIALS_FOR_RESOURCE)
        .bind(id)
        .execute(&state.db)
        .await?;
    // delete the resource itself
    sqlx::query(sql::DELETE_RESOURCE_BY_ID)
        .bind(id)
        .execute(&state.db)
        .await?;
    let message = format!(
        "{} deleted resource {id} (name: {}, category: {})",
        user_info.cid, resource.name, resource.category
    );
    record_log(message.clone(), &state.db, true).await?;
    post_audit(&state.config, message);
    Ok(StatusCode::OK)
}

/// Load a list of controllers who have signed off on the resource.
async fn api_get_resource_initials(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(id): Path<u32>,
) -> Result<Response, AppError> {
    #[derive(Serialize)]
    struct ControllerInfo {
        cid: u32,
        name: String,
        created_date: DateTime<Utc>,
    }

    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if !is_user_member_of(&state, &user_info, PermissionsGroup::NamedPosition).await {
        return Ok(StatusCode::FORBIDDEN.into_response());
    }
    let user_info = user_info.unwrap();
    let resource: Option<Resource> = sqlx::query_as(sql::GET_RESOURCE_BY_ID)
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    let resource = match resource {
        Some(r) => r,
        None => {
            warn!(
                "{} tried to get SOP initials on unknown resource {id}",
                user_info.cid
            );
            return Ok(StatusCode::NOT_FOUND.into_response());
        }
    };
    let initials: Vec<SopInitial> = sqlx::query_as(sql::GET_SOP_INITIALS_FOR_RESOURCE)
        .bind(resource.id)
        .fetch_all(&state.db)
        .await?;
    let all_controllers = get_controller_cids_and_names(&state.db)
        .await
        .map_err(|e| AppError::GenericFallback("getting names and CIDs from DB", e))?;
    let data: Vec<ControllerInfo> = initials
        .iter()
        .map(|entry| {
            let name = all_controllers
                .get(&entry.cid)
                .map(|e| e.to_owned())
                .unwrap_or_else(|| (String::new(), String::new()));
            ControllerInfo {
                cid: entry.cid,
                name: format!("{} {}", name.0, name.1),
                created_date: entry.created_date,
            }
        })
        .collect();
    let template = state
        .templates
        .get_template("admin/resources_initials.jinja")?;
    let rendered = template.render(context! { user_info, data })?;
    Ok(Html(rendered).into_response())
}

/// Form submission for creating a new resource.
///
/// Named staff members only.
async fn post_new_resource(
    State(state): State<Arc<AppState>>,
    session: Session,
    mut form: Multipart,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) =
        reject_if_not_in(&state, &user_info, PermissionsGroup::NamedPosition).await
    {
        return Ok(redirect);
    }
    let user_info = user_info.unwrap();
    let mut resource = Resource {
        updated: Utc::now(),
        ..Default::default()
    };

    // have to use a `Multipart` struct for this, so loop through it to get what the data
    while let Some(field) = form.next_field().await? {
        let name = field.name().ok_or(AppError::MultipartFormGet)?.to_string();
        match name.as_str() {
            "name" => {
                resource.name = field.text().await?;
            }
            "category" => {
                resource.category = field.text().await?;
            }
            "file" => {
                let new_uuid = Uuid::new_v4();
                let file_name = field
                    .file_name()
                    .ok_or(AppError::MultipartFormGet)?
                    .to_string();
                let file_data = field.bytes().await?;
                let new_file_name = format!("{new_uuid}_{file_name}");
                let write_path = FilePath::new("./assets").join(&new_file_name);
                debug!(
                    "Writing new file to assets dir as part of resource upload: {new_file_name}"
                );
                std::fs::write(write_path, file_data)?;
                resource.file_name = Some(new_file_name);
            }
            "link" => {
                resource.link = Some(field.text().await?);
            }
            _ => {}
        }
    }

    // save the constructed struct fields
    sqlx::query(sql::CREATE_NEW_RESOURCE)
        .bind(&resource.category)
        .bind(&resource.name)
        .bind(resource.file_name)
        .bind(resource.link)
        .bind(resource.updated)
        .execute(&state.db)
        .await?;
    flashed_messages::push_flashed_message(session, MessageLevel::Info, "New resource created")
        .await?;
    let message = format!(
        "{} created a new resource name: {}, category: {}",
        user_info.cid, resource.name, resource.category,
    );
    record_log(message.clone(), &state.db, true).await?;
    post_audit(&state.config, message);
    Ok(Redirect::to("/admin/resources"))
}

/// Edit an existing resource with new information.
async fn api_edit_resource(
    State(state): State<Arc<AppState>>,
    session: Session,
    mut form: Multipart,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) =
        reject_if_not_in(&state, &user_info, PermissionsGroup::NamedPosition).await
    {
        return Ok(redirect);
    }
    let user_info = user_info.unwrap();
    let mut resource = Resource::default();

    while let Some(field) = form.next_field().await? {
        let name = field.name().ok_or(AppError::MultipartFormGet)?.to_string();
        match name.as_str() {
            "id" => resource.id = field.text().await?.parse()?,
            "name" => resource.name = field.text().await?,
            "category" => resource.category = field.text().await?,
            "file" => {
                let new_uuid = Uuid::new_v4();
                let file_name = field
                    .file_name()
                    .ok_or(AppError::MultipartFormGet)?
                    .to_string();
                let file_data = field.bytes().await?;
                if !file_data.is_empty() {
                    let new_file_name = format!("{new_uuid}_{file_name}");
                    let write_path = FilePath::new("./assets").join(&new_file_name);
                    debug!(
                        "Writing new file to assets dir as part of resource upload: {new_file_name}"
                    );
                    std::fs::write(write_path, file_data)?;
                    resource.file_name = Some(new_file_name);
                }
            }
            "link" => resource.link = Some(field.text().await?),
            _ => {}
        }
    }

    // update the DB record
    sqlx::query(sql::UPDATE_RESOURCE)
        .bind(resource.id)
        .bind(&resource.category)
        .bind(&resource.name)
        .bind(resource.file_name)
        .bind(resource.link)
        .bind(Utc::now())
        .execute(&state.db)
        .await?;

    // delete all access records for this SOP
    sqlx::query(sql::DELETE_SOP_ACCESS_FOR_RESOURCE)
        .bind(resource.id)
        .execute(&state.db)
        .await?;
    // delete all initials for this SOP
    sqlx::query(sql::DELETE_SOP_INITIALS_FOR_RESOURCE)
        .bind(resource.id)
        .execute(&state.db)
        .await?;

    // record the update
    let update_message = format!(
        "{} updated resource {} (name: {}, category: {})",
        user_info.cid, resource.id, resource.name, resource.category
    );
    record_log(update_message.clone(), &state.db, true).await?;
    post_audit(&state.config, update_message);
    flashed_messages::push_flashed_message(
        session,
        flashed_messages::MessageLevel::Success,
        "Resource updated",
    )
    .await?;

    Ok(Redirect::to("/admin/resources"))
}

/// Page for controllers that are not on the roster but have controller DB entries.
///
/// Any staff members only.
async fn page_off_roster_list(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::SomeStaff).await
    {
        return Ok(redirect.into_response());
    }
    let controllers: Vec<Controller> = sqlx::query_as(sql::GET_ALL_CONTROLLERS_OFF_ROSTER)
        .fetch_all(&state.db)
        .await?;
    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let template = state
        .templates
        .get_template("admin/off_roster_list.jinja")?;
    let rendered = template.render(context! {
       user_info,
       controllers,
       flashed_messages
    })?;
    Ok(Html(rendered).into_response())
}

/// Simple page with controls to render the activity report.
///
/// Admin staff members only.
async fn page_activity_report(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect.into_response());
    }
    let months = sqlx::query(sql::SELECT_ACTIVITY_JUST_MONTHS)
        .fetch_all(&state.db)
        .await?;
    let months: Vec<String> = months
        .iter()
        .map(|row| row.try_get("month").unwrap())
        .sorted()
        .rev()
        .collect();
    let template = state
        .templates
        .get_template("admin/activity_report_container.jinja")?;
    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let rendered = template.render(context! { user_info, months, flashed_messages })?;
    Ok(Html(rendered).into_response())
}

#[derive(Debug, Deserialize)]
struct ReportParams {
    month: Vec<String>,
}

/// Page to render the activity report.
///
/// May take up to ~30 seconds to load, so will be loaded into the container page
/// as part of an HTMX action. Cached.
///
/// Admin staff members only.
async fn page_activity_report_generate(
    State(state): State<Arc<AppState>>,
    session: Session,
    months: Query<ReportParams>,
) -> Result<Response, AppError> {
    #[derive(Serialize)]
    struct BasicInfo {
        cid: u32,
        name: String,
        join_date: Option<DateTime<Utc>>,
        home: bool,
        minutes_online: u32,
    }

    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect.into_response());
    }
    let user_info = user_info.unwrap();

    // cache this endpoint's returned data for 3 hours
    let cache_key = "ACTIVITY_REPORT".to_string();
    if let Some(cached) = state.cache.get(&cache_key) {
        let elapsed = Instant::now() - cached.inserted;
        if elapsed.as_secs() < 60 * 60 * 3 {
            return Ok(Html(cached.data).into_response());
        }
        state.cache.invalidate(&cache_key);
    }

    record_log(
        format!("{} generating activity report", user_info.cid),
        &state.db,
        true,
    )
    .await?;

    debug!("Getting activity from DB");
    let months = &months.month;
    let controllers: Vec<Controller> = sqlx::query_as(sql::GET_ALL_CONTROLLERS_ON_ROSTER)
        .fetch_all(&state.db)
        .await?;
    let activity: Vec<Activity> = sqlx::query_as(sql::GET_ALL_ACTIVITY)
        .fetch_all(&state.db)
        .await?;
    let activity_map: HashMap<u32, u32> =
        activity.iter().fold(HashMap::new(), |mut acc, activity| {
            if !months.contains(&activity.month) {
                return acc;
            }
            acc.entry(activity.cid)
                .and_modify(|entry| *entry += activity.minutes)
                .or_insert(activity.minutes);
            acc
        });
    debug!("Determining violations");
    let rated_violations: Vec<BasicInfo> = controllers
        .iter()
        .filter(|controller| {
            controller.rating > ControllerRating::OBS.as_id()
                && activity_map.get(&controller.cid).unwrap_or(&0) < &180
        })
        .map(|controller| BasicInfo {
            cid: controller.cid,
            name: format!(
                "{} {} ({})",
                controller.first_name,
                controller.last_name,
                match &controller.operating_initials {
                    Some(oi) => oi,
                    None => "??",
                }
            ),
            join_date: controller.join_date,
            home: controller.home_facility == "ZDV",
            minutes_online: *activity_map.get(&controller.cid).unwrap_or(&0),
        })
        .collect();

    debug!("Querying training records for OBS controllers");
    let mut unrated_violations: Vec<BasicInfo> = Vec::new();
    for controller in &controllers {
        if controller.rating != ControllerRating::OBS.as_id() {
            continue;
        }
        let records =
            match vatusa::get_training_records(controller.cid, &state.config.vatsim.vatusa_api_key)
                .await
            {
                Ok(t) => t,
                Err(e) => {
                    error!(
                        "Error getting training record data for {}: {e}",
                        controller.cid
                    );
                    continue;
                }
            };
        let in_time_frame = records
            .iter()
            .filter(|r| {
                let month = &r.session_date[0..7];
                months.contains(&month.to_string())
            })
            .count();
        if in_time_frame == 0 {
            unrated_violations.push(BasicInfo {
                cid: controller.cid,
                name: format!(
                    "{} {} ({})",
                    controller.first_name,
                    controller.last_name,
                    match &controller.operating_initials {
                        Some(oi) => oi,
                        None => "??",
                    }
                ),
                join_date: controller.join_date,
                home: controller.home_facility == "ZDV",
                minutes_online: 0,
            });
        }
    }

    info!("Returning activity report");
    let template = state
        .templates
        .get_template("admin/activity_report.jinja")?;
    let rendered = template.render(context! {
        user_info,
        controllers,
        rated_violations,
        unrated_violations,
        months => months.iter().join(", "),
        now_utc => Utc::now().to_rfc2822(),
    })?;
    state
        .cache
        .insert(cache_key, CacheEntry::new(rendered.clone()));
    Ok(Html(rendered).into_response())
}

/// Clear the existing activity report out of the cache.
async fn page_activity_report_delete(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect);
    }
    state.cache.invalidate(&"ACTIVITY_REPORT".to_string());
    record_log(
        format!("{} deleted the activity report", user_info.unwrap().cid),
        &state.db,
        true,
    )
    .await?;
    flashed_messages::push_flashed_message(
        session,
        MessageLevel::Success,
        "Activity report deleted",
    )
    .await?;
    Ok(Redirect::to("/admin/activity_report"))
}

/// Remove the specified controllers from the facility.
///
/// Admin members only.
async fn page_activity_report_roster_remove(
    State(state): State<Arc<AppState>>,
    session: Session,
    Json(cids): Json<Vec<u32>>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect);
    }
    if cids.is_empty() {
        push_flashed_message(session, MessageLevel::Error, "No controllers selected").await?;
        return Ok(Redirect::to("/admin/activity_report"));
    }
    let user_info = user_info.unwrap();

    for cid in cids {
        if let Err(e) = remove_controller_from_roster(
            cid,
            user_info.cid,
            "Did not meet activity requirements per Facility Policy",
            &state.db,
            &state.config,
        )
        .await
        {
            error!("Error removing controller {cid} from roster, activity page: {e}");
        }
    }

    push_flashed_message(session, MessageLevel::Success, "Removals processed").await?;
    Ok(Redirect::to("/admin/activity_report"))
}

/// Page to centrally view all active solo certs.
///
/// All staff members only.
async fn page_solo_cert_list(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::SomeStaff).await
    {
        return Ok(redirect.into_response());
    }
    let user_info = user_info.unwrap();
    let solo_certs: Vec<SoloCert> = sqlx::query_as(sql::GET_ALL_SOLO_CERTS)
        .fetch_all(&state.db)
        .await?;
    let cids_and_names = get_controller_cids_and_names(&state.db)
        .await
        .map_err(|err| AppError::GenericFallback("getting cids and names from DB", err))?;
    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let template = state.templates.get_template("admin/solo_cert_list.jinja")?;
    let rendered = template.render(context! {
        user_info,
        flashed_messages,
        solo_certs,
        cids_and_names
    })?;
    Ok(Html(rendered).into_response())
}

/// Page for listing and adding no-shows for training and events.
///
/// For any staff member.
async fn page_no_show_list(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::SomeStaff).await
    {
        return Ok(redirect.into_response());
    }
    let user_info = user_info.unwrap();

    let (filtering, no_shows) = {
        let no_shows: Vec<NoShow> = sqlx::query_as(sql::GET_ALL_NO_SHOW)
            .fetch_all(&state.db)
            .await?;

        if user_info.is_admin || (user_info.is_event_staff && user_info.is_training_staff) {
            ("all", no_shows)
        } else {
            let filter = if user_info.is_event_staff {
                "event"
            } else if user_info.is_training_staff {
                "training"
            } else {
                "none"
            };
            (
                filter,
                no_shows
                    .iter()
                    .filter(|ns| ns.entry_type == filter)
                    .map(|ns| ns.to_owned())
                    .collect(),
            )
        }
    };

    let all_controllers: Vec<Controller> = sqlx::query_as(sql::GET_ALL_CONTROLLERS)
        .fetch_all(&state.db)
        .await?;
    let all_controllers: Vec<(u32, String)> = all_controllers
        .iter()
        .map(|controller| {
            (
                controller.cid,
                format!(
                    "{} {} ({})",
                    controller.first_name,
                    controller.last_name,
                    match controller.operating_initials.as_ref() {
                        Some(oi) => {
                            if oi.is_empty() { "??" } else { oi }
                        }
                        None => "??",
                    }
                ),
            )
        })
        .collect();
    let cid_name_name = get_controller_cids_and_names(&state.db)
        .await
        .map_err(|err| AppError::GenericFallback("getting cids and names from DB", err))?;
    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let template = state.templates.get_template("admin/no_show_list.jinja")?;
    let rendered = template.render(context! {
        user_info,
        flashed_messages,
        all_controllers,
        cid_name_name,
        filtering,
        no_shows
    })?;
    Ok(Html(rendered).into_response())
}

#[derive(Debug, Deserialize)]
struct NewNoShowForm {
    controller: Option<u32>,
    entry_type: Option<String>,
    notes: Option<String>,
}

/// Submit a new no-show entry.
///
/// For any staff member.
async fn post_new_no_show(
    State(state): State<Arc<AppState>>,
    session: Session,
    Form(new_entry_form): Form<NewNoShowForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::SomeStaff).await
    {
        return Ok(redirect);
    }
    let user_info = user_info.unwrap();

    if new_entry_form.controller.is_none() {
        flashed_messages::push_flashed_message(
            session,
            MessageLevel::Error,
            "Controller not selected",
        )
        .await?;
        return Ok(Redirect::to("/admin/no_show_list"));
    }
    if new_entry_form.entry_type.is_none() {
        flashed_messages::push_flashed_message(session, MessageLevel::Error, "Type not selected")
            .await?;
        return Ok(Redirect::to("/admin/no_show_list"));
    }

    let matching_controller: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
        .bind(new_entry_form.controller)
        .fetch_optional(&state.db)
        .await?;
    if matching_controller.is_none() {
        flashed_messages::push_flashed_message(
            session,
            MessageLevel::Error,
            "No matching controller found",
        )
        .await?;
        return Ok(Redirect::to("/admin/no_show_list"));
    }

    sqlx::query(sql::CREATE_NEW_NO_SHOW_ENTRY)
        .bind(new_entry_form.controller.unwrap())
        .bind(user_info.cid)
        .bind(&new_entry_form.entry_type)
        .bind(Utc::now())
        .bind(&new_entry_form.notes)
        .execute(&state.db)
        .await?;
    flashed_messages::push_flashed_message(session, MessageLevel::Success, "Entry added").await?;
    record_log(
        format!(
            "{} added new no-show entry for {} of {}",
            user_info.cid,
            new_entry_form.controller.unwrap(),
            new_entry_form.entry_type.unwrap()
        ),
        &state.db,
        true,
    )
    .await?;
    Ok(Redirect::to("/admin/no_show_list"))
}

/// API endpoint to delete a no-show entry.
///
/// For any staff member.
async fn api_delete_no_show_entry(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(id): Path<u32>,
) -> Result<StatusCode, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if reject_if_not_in(&state, &user_info, PermissionsGroup::SomeStaff)
        .await
        .is_some()
    {
        return Ok(StatusCode::FORBIDDEN);
    }
    let user_info = user_info.unwrap();

    let no_show_entry: Option<NoShow> = sqlx::query_as(sql::GET_NO_SHOW_BY_ID)
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    let no_show_entry = match no_show_entry {
        Some(e) => e,
        None => {
            flashed_messages::push_flashed_message(
                session,
                MessageLevel::Error,
                "No entry with that ID found",
            )
            .await?;
            warn!(
                "{} tried to delete unknown no-show entry {}",
                user_info.cid, id
            );
            return Ok(StatusCode::NOT_FOUND);
        }
    };
    sqlx::query(sql::DELETE_NO_SHOW_ENTRY)
        .bind(id)
        .execute(&state.db)
        .await?;
    flashed_messages::push_flashed_message(session, MessageLevel::Success, "Entry deleted").await?;
    record_log(
        format!(
            "{} deleted no-show entry #{} for {} of {} from {}",
            user_info.cid,
            id,
            no_show_entry.cid,
            no_show_entry.entry_type,
            no_show_entry.reported_by
        ),
        &state.db,
        true,
    )
    .await?;

    Ok(StatusCode::OK)
}

/// Show a log of important events.
///
/// Admin staff members only.
async fn page_audit_log(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect.into_response());
    }

    let logs: Vec<Log> = sqlx::query_as(sql::GET_ALL_LOGS)
        .fetch_all(&state.db)
        .await?;
    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let template = state.templates.get_template("admin/audit_log.jinja")?;
    let rendered = template.render(context! {
        user_info,
        flashed_messages,
        logs
    })?;
    Ok(Html(rendered).into_response())
}

/// This file's routes and templates.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/admin/feedback",
            get(page_feedback).post(post_feedback_form_handle),
        )
        .route(
            "/admin/feedback/edited",
            post(post_feedback_edited_form_handle),
        )
        .route("/admin/emails", get(page_emails))
        .route("/admin/emails/update", post(post_email_template_update))
        .route("/admin/emails/send", post(post_email_manual_send))
        .route("/admin/logs", get(page_logs))
        .route(
            "/admin/visitor_applications",
            get(page_visitor_applications),
        )
        .route(
            "/admin/visitor_applications/{id}",
            get(post_visitor_application_action),
        )
        .route(
            "/admin/resources",
            get(page_resources).post(post_new_resource),
        )
        .layer(DefaultBodyLimit::disable()) // no upload limit on this endpoint
        .route("/admin/resources/{id}", delete(api_delete_resource))
        .route(
            "/admin/resources/{id}/initials",
            get(api_get_resource_initials),
        )
        .route("/admin/resources/edit", post(api_edit_resource))
        .layer(DefaultBodyLimit::disable()) // no upload limit on this endpoint
        .route("/admin/off_roster_list", get(page_off_roster_list))
        .route("/admin/activity_report", get(page_activity_report))
        .route(
            "/admin/activity_report/generate",
            get(page_activity_report_generate),
        )
        .route(
            "/admin/activity_report/delete",
            get(page_activity_report_delete),
        )
        .route(
            "/admin/activity_report/roster_remove",
            post(page_activity_report_roster_remove),
        )
        .route("/admin/solo_cert_list", get(page_solo_cert_list))
        .route(
            "/admin/no_show_list",
            get(page_no_show_list).post(post_new_no_show),
        )
        .route("/admin/no_show_list/{id}", delete(api_delete_no_show_entry))
        .route("/admin/audit_log", get(page_audit_log))
}
