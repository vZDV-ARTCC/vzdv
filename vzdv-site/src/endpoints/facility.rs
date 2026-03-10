//! Endpoints for getting information on the facility.

use crate::{
    flashed_messages::{self, MessageLevel, push_flashed_message},
    shared::{AppError, AppState, CacheEntry, SESSION_USER_INFO_KEY, UserInfo, record_log},
    vatusa,
};
use axum::{
    Form, Router,
    extract::State,
    response::{Html, Json, Redirect},
    routing::get,
};
use chrono::{DateTime, Months, Utc};
use indexmap::IndexMap;
use itertools::Itertools;
use log::{error, info, warn};
use minijinja::context;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};
use tower_sessions::Session;
use vzdv::{
    ControllerRating, GENERAL_HTTP_CLIENT,
    activity::get_controller_activity,
    config::Config,
    determine_staff_positions,
    sql::{self, Certification, Controller, Resource, SopAccess, SopInitial, VisitorRequest},
};

#[derive(Debug, Serialize)]
struct StaffPosition {
    short: &'static str,
    name: &'static str,
    order: u8,
    controllers: Vec<Controller>,
    email: Option<String>,
    description: &'static str,
}

type ParsedAlias = Vec<(String, Vec<(String, Vec<String>)>)>;

fn generate_staff_outline(config: &Config) -> HashMap<&'static str, StaffPosition> {
    let email_domain = &config.staff.email_domain;
    HashMap::from([
        (
            "ATM",
            StaffPosition {
                short: "ATM",
                name: "Air Traffic Manager",
                order: 1,
                controllers: Vec::new(),
                email: Some(format!("atm@{email_domain}")),
                description: "Responsible for the macro-management of the facility. Oversees day-to-day operations and ensures that the facility is running smoothly.",
            },
        ),
        (
            "DATM",
            StaffPosition {
                short: "DATM",
                name: "Deputy Air Traffic Manager",
                order: 2,
                controllers: Vec::new(),
                email: Some(format!("datm@{email_domain}")),
                description: "Assists the Air Traffic Manager with the management of the facility. Acts as the Air Traffic Manager in their absence.",
            },
        ),
        (
            "TA",
            StaffPosition {
                short: "TA",
                name: "Training Administrator",
                order: 3,
                controllers: Vec::new(),
                email: Some(format!("ta@{email_domain}")),
                description: "Responsible for overseeing and management of the facility's training program and staff.",
            },
        ),
        (
            "FE",
            StaffPosition {
                short: "FE",
                name: "Facility Engineer",
                order: 4,
                controllers: Vec::new(),
                email: Some(format!("fe@{email_domain}")),
                description: "Responsible for the creation of sector files, radar client files, and other facility resources.",
            },
        ),
        (
            "EC",
            StaffPosition {
                short: "EC",
                name: "Events Coordinator",
                order: 5,
                controllers: Vec::new(),
                email: Some(format!("ec@{email_domain}")),
                description: "Responsible for the planning, coordination and advertisement of facility events with neighboring facilities, virtual airlines, VATUSA, and VATSIM.",
            },
        ),
        (
            "WM",
            StaffPosition {
                short: "WM",
                name: "Webmaster",
                order: 6,
                controllers: Vec::new(),
                email: Some(format!("wm@{email_domain}")),
                description: "Responsible for the management of the facility's website and technical infrastructure.",
            },
        ),
        (
            "ATA",
            StaffPosition {
                short: "ATA",
                name: "Assistant Training Administrator",
                order: 7,
                controllers: Vec::new(),
                email: None,
                description: "Assists the Training Administrator and assumes all duties of an Instructor.",
            },
        ),
        (
            "INS",
            StaffPosition {
                short: "INS",
                name: "Instructor",
                order: 8,
                controllers: Vec::new(),
                email: None,
                description: "Under direction of the Training Administrator, leads training and handles RCEs.",
            },
        ),
        (
            "MTR",
            StaffPosition {
                short: "MTR",
                name: "Mentor",
                order: 9,
                controllers: Vec::new(),
                email: None,
                description: "Under direction of the Training Administrator, helps train students and prepare them for RCEs.",
            },
        ),
        (
            "AFE",
            StaffPosition {
                short: "AFE",
                name: "Assistant Facility Engineer",
                order: 10,
                controllers: Vec::new(),
                email: None,
                description: "Assists the Facility Engineer.",
            },
        ),
        (
            "AEC",
            StaffPosition {
                short: "AEC",
                name: "Assistant Events Coordinator",
                order: 11,
                controllers: Vec::new(),
                email: None,
                description: "Assists the Events Coordinator.",
            },
        ),
        (
            "AWM",
            StaffPosition {
                short: "AWM",
                name: "Assistant Webmaster",
                order: 12,
                controllers: Vec::new(),
                email: None,
                description: "Assists the Webmaster.",
            },
        ),
    ])
}

#[derive(Debug, Serialize)]
struct ControllerWithCerts<'a> {
    cid: u32,
    first_name: &'a str,
    last_name: &'a str,
    operating_initials: &'a str,
    rating: &'static str,
    is_home: bool,
    roles: String,
    certs: Vec<Certification>,
    loa_until: Option<DateTime<Utc>>,
}

/// View the full roster.
async fn page_roster(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let controllers: Vec<Controller> = sqlx::query_as(sql::GET_ALL_CONTROLLERS_ON_ROSTER)
        .fetch_all(&state.db)
        .await?;
    let certifications: Vec<Certification> = sqlx::query_as(sql::GET_ALL_CERTIFICATIONS)
        .fetch_all(&state.db)
        .await?;

    let certification_order = &state.config.training.certifications;
    let cert_order_map: HashMap<&String, usize> = certification_order
        .iter()
        .enumerate()
        .map(|(index, cert)| (cert, index))
        .collect();

    let controllers_with_certs: Vec<_> = controllers
        .iter()
        .map(|controller| {
            let operating_initials = match &controller.operating_initials {
                Some(s) => s,
                None => "",
            };
            let roles = determine_staff_positions(controller).join(", ");
            let mut certs = certifications
                .iter()
                .filter(|cert| cert.cid == controller.cid)
                .cloned()
                .collect::<Vec<_>>();

            // Sort certifications based on the order in the TOML file
            certs.sort_by_key(|cert| {
                cert_order_map
                    .get(&cert.name)
                    .cloned()
                    .unwrap_or(usize::MAX)
            });

            ControllerWithCerts {
                cid: controller.cid,
                first_name: &controller.first_name,
                last_name: &controller.last_name,
                operating_initials,
                rating: ControllerRating::try_from(controller.rating)
                    .map(|r| r.as_str())
                    .unwrap_or(""),
                is_home: controller.home_facility == "ZDV",
                roles,
                certs,
                loa_until: controller.loa_until,
            }
        })
        .sorted_by(|a, b| Ord::cmp(&a.cid, &b.cid))
        .collect();

    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let template = state.templates.get_template("facility/roster.jinja")?;
    let rendered = template.render(context! {
       user_info,
       controllers => controllers_with_certs,
       flashed_messages
    })?;
    Ok(Html(rendered))
}

/// View some stats about the roster.
async fn page_roster_stats(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let template = state
        .templates
        .get_template("facility/roster_stats.jinja")?;
    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let rendered = template.render(context! { user_info, flashed_messages })?;
    Ok(Html(rendered))
}

#[derive(Serialize)]
struct NameValuePair<'a> {
    name: &'a str,
    value: u16,
}

/// View some stats about the roster (JSON API endpoint).
async fn api_roster_stats(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    // cache this endpoint's returned data for 5 minutes
    let cache_key = "ROSTER_STATS".to_string();
    if let Some(cached) = state.cache.get(&cache_key) {
        let elapsed = Instant::now() - cached.inserted;
        if elapsed.as_secs() < 300 {
            return Ok(Json(serde_json::from_str(&cached.data).unwrap()));
        }
        state.cache.invalidate(&cache_key);
    }

    let controllers: Vec<Controller> = sqlx::query_as(sql::GET_ALL_CONTROLLERS_ON_ROSTER)
        .fetch_all(&state.db)
        .await?;
    let controllers: Vec<_> = controllers
        .iter()
        .filter(|c| c.home_facility == "ZDV")
        .collect();
    let home_controllers: HashSet<u32> = controllers.iter().map(|c| c.cid).collect();
    let certifications: Vec<Certification> = sqlx::query_as(sql::GET_ALL_CERTIFICATIONS)
        .fetch_all(&state.db)
        .await?;

    let mut by_rating: HashMap<&str, u16> = HashMap::new();
    controllers.iter().for_each(|c| {
        by_rating
            .entry(ControllerRating::try_from(c.rating).unwrap().as_str())
            .and_modify(|i| *i += 1)
            .or_insert(1);
    });
    let by_rating: Vec<_> = by_rating
        .iter()
        .map(|(&name, &value)| NameValuePair { name, value })
        .collect();

    let mut by_cert: HashMap<&str, u16> = HashMap::new();
    certifications
        .iter()
        .filter(|c| home_controllers.contains(&c.cid) && c.value == "certified")
        .for_each(|c| {
            by_cert.entry(&c.name).and_modify(|i| *i += 1).or_insert(1);
        });
    let certs = &state.config.training.certifications;
    let by_cert: Vec<_> = by_cert
        .iter()
        .map(|(&name, &value)| (name, value))
        .sorted_by(|a, b| {
            let a = certs.iter().position(|e| e == a.0).unwrap_or_default();
            let b = certs.iter().position(|e| e == b.0).unwrap_or_default();
            Ord::cmp(&a, &b)
        })
        .map(|(_, value)| value)
        .collect();

    let data = json!({ "by_rating": by_rating, "certs": certs, "by_cert": by_cert });
    state.cache.insert(
        cache_key,
        CacheEntry::new(serde_json::to_string(&data).unwrap()),
    );
    Ok(Json(data))
}

/// View the facility's staff.
async fn page_staff(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    let mut staff_map = generate_staff_outline(&state.config);
    let controllers: Vec<Controller> = sqlx::query_as(sql::GET_ALL_CONTROLLERS)
        .fetch_all(&state.db)
        .await?;
    for controller in &controllers {
        let roles = determine_staff_positions(controller);
        for role in roles {
            if let Some(staff_pos) = staff_map.get_mut(role.as_str()) {
                staff_pos.controllers.push(controller.clone());
            } else {
                warn!("No staff role found for: {role}");
            }
        }
    }

    let staff: Vec<_> = staff_map
        .values()
        .sorted_by(|a, b| Ord::cmp(&a.order, &b.order))
        .collect();

    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let template = state.templates.get_template("facility/staff.jinja")?;
    let rendered = template.render(context! { user_info, staff })?;
    Ok(Html(rendered))
}

/// View all controller's recent (summarized) controlling activity.
async fn page_activity(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    // time ranges
    let now = Utc::now();
    let months = vec![
        now.format("%Y-%m").to_string(),
        now.checked_sub_months(Months::new(1))
            .unwrap()
            .format("%Y-%m")
            .to_string(),
        now.checked_sub_months(Months::new(2))
            .unwrap()
            .format("%Y-%m")
            .to_string(),
        now.checked_sub_months(Months::new(3))
            .unwrap()
            .format("%Y-%m")
            .to_string(),
        now.checked_sub_months(Months::new(4))
            .unwrap()
            .format("%Y-%m")
            .to_string(),
    ];

    let mut activity_data = get_controller_activity(&state.db, &months)
        .await
        .map_err(|e| AppError::GenericFallback("error getting activity from database", e))?;

    // top 3 controllers for each month
    for month in 0..=4 {
        activity_data
            .iter()
            .enumerate()
            .map(|(index, data)| (index, data.months[month].value))
            .sorted_by(|a, b| Ord::cmp(&b.1, &a.1))
            .map(|(index, _data)| index)
            .take(3)
            .enumerate()
            .for_each(|(rank, controller_index)| {
                activity_data[controller_index].months[month].position = Some(rank as u8);
            });
    }

    // summary row for the bottom
    let totals = activity_data.iter().fold((0, 0, 0, 0, 0), |acc, row| {
        (
            acc.0 + row.months.first().map(|am| am.value).unwrap_or_default(),
            acc.1 + row.months.get(1).map(|am| am.value).unwrap_or_default(),
            acc.2 + row.months.get(2).map(|am| am.value).unwrap_or_default(),
            acc.3 + row.months.get(3).map(|am| am.value).unwrap_or_default(),
            acc.4 + row.months.get(4).map(|am| am.value).unwrap_or_default(),
        )
    });

    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let template = state.templates.get_template("facility/activity.jinja")?;
    let rendered = template.render(context! { user_info, activity_data, totals })?;
    Ok(Html(rendered))
}

/// View files uploaded to the site.
async fn page_resources(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    let resources: Vec<Resource> = sqlx::query_as(sql::GET_ALL_RESOURCES)
        .fetch_all(&state.db)
        .await?;
    let resources: Vec<_> = resources
        .iter()
        .sorted_by(|a, b| a.name.cmp(&b.name))
        .collect();

    let categories: Vec<_> = resources
        .iter()
        .map(|r| &r.category)
        .collect::<HashSet<_>>()
        .into_iter()
        .sorted()
        .collect();
    let categories: Vec<_> = state
        .config
        .database
        .resource_category_ordering
        .iter()
        .filter(|category| categories.contains(category))
        .collect();

    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let sop_initials: HashMap<u32, bool> = match user_info {
        Some(ref ui) => {
            let initials: Vec<SopInitial> = sqlx::query_as(sql::GET_ALL_SOP_INITIALS_FOR_CID)
                .bind(ui.cid)
                .fetch_all(&state.db)
                .await?;
            initials.iter().fold(HashMap::new(), |mut acc, item| {
                acc.insert(item.resource_id, true);
                acc
            })
        }
        None => HashMap::new(),
    };

    let template = state.templates.get_template("facility/resources.jinja")?;
    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let rendered = template.render(context! {
        user_info,
        resources,
        categories,
        flashed_messages,
        sop_initials
    })?;
    Ok(Html(rendered))
}

#[derive(Debug, Deserialize)]
struct SignSopForm {
    resource_id: u32,
}

/// Form submission handler for a controller signing an SOP resource.
async fn post_page_resources_sign(
    State(state): State<Arc<AppState>>,
    session: Session,
    Form(sign_form): Form<SignSopForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let user_info = match user_info {
        Some(ui) => ui,
        None => {
            return Ok(Redirect::to("/facility/resources"));
        }
    };
    let resource: Option<Resource> = sqlx::query_as(sql::GET_RESOURCE_BY_ID)
        .bind(sign_form.resource_id)
        .fetch_optional(&state.db)
        .await?;
    let resource = match resource {
        Some(r) => r,
        None => {
            push_flashed_message(session, MessageLevel::Error, "Unknown resource").await?;
            return Ok(Redirect::to("/facility/resources"));
        }
    };
    if resource.category != "SOPs" {
        push_flashed_message(
            session,
            MessageLevel::Error,
            "You cannot sign non-SOP resources",
        )
        .await?;
        return Ok(Redirect::to("/facility/resources"));
    }

    let access_record: Option<SopAccess> = sqlx::query_as(sql::GET_SOP_ACCESS_FOR_CID_AND_RESOURCE)
        .bind(user_info.cid)
        .bind(resource.id)
        .fetch_optional(&state.db)
        .await?;
    if access_record.is_none() {
        push_flashed_message(
            session,
            MessageLevel::Error,
            "There is no record of you having opened this document",
        )
        .await?;
        return Ok(Redirect::to("/facility/resources"));
    }

    sqlx::query(sql::INSERT_SOP_INITIALS)
        .bind(user_info.cid)
        .bind(resource.id)
        .bind(Utc::now())
        .execute(&state.db)
        .await?;
    push_flashed_message(session, MessageLevel::Success, "Resource signed").await?;
    record_log(
        format!(
            "{} signed resource {}, '{}'",
            user_info.cid, resource.id, resource.name
        ),
        &state.db,
        false,
    )
    .await?;
    Ok(Redirect::to("/facility/resources"))
}

pub async fn fetch_and_parse_alias_file() -> Result<ParsedAlias, reqwest::Error> {
    let url = "https://data-api.vnas.vatsim.net/Files/Aliases/ZDV.txt";
    let response = reqwest::get(url).await?.text().await?;

    let mut parsed_data: IndexMap<String, IndexMap<String, Vec<String>>> = IndexMap::new();
    let mut current_h1 = String::new();
    let mut current_h2 = String::new();

    for line in response.lines() {
        if line.starts_with(";;;;") {
            // New Heading 1
            current_h1 = line.strip_prefix(";;;;").unwrap_or(line).trim().to_string();
            parsed_data.entry(current_h1.clone()).or_default();
            current_h2 = String::new(); // Reset H2
        } else if line.starts_with(";;;") {
            // New Heading 2
            current_h2 = line.strip_prefix(";;;").unwrap_or(line).trim().to_string();
            parsed_data
                .entry(current_h1.clone())
                .or_default()
                .entry(current_h2.clone())
                .or_default();
        } else if line.starts_with('.') {
            // Command under current H1 or H2
            if !current_h1.is_empty() {
                if !current_h2.is_empty() {
                    parsed_data
                        .entry(current_h1.clone())
                        .or_default()
                        .entry(current_h2.clone())
                        .or_default()
                        .push(line.trim().to_string());
                } else {
                    // Command directly under H1
                    parsed_data
                        .entry(current_h1.clone())
                        .or_default()
                        .entry("__root__".to_string())
                        .or_default()
                        .push(line.trim().to_string());
                }
            }
        }
    }

    // Convert IndexMap to Vec for Jinja compatibility
    let parsed_vec: ParsedAlias = parsed_data
        .into_iter()
        .map(|(h1, h2_map)| {
            let h2_vec = h2_map.into_iter().collect();
            (h1, h2_vec)
        })
        .collect();

    Ok(parsed_vec)
}

/// View Alias commands for the facility. (Polled from the vNAS API)
async fn alias_ref(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let template = state.templates.get_template("facility/aliasref.jinja")?;
    let alias_ref = fetch_and_parse_alias_file().await?;
    let rendered = template.render(context! { user_info, alias_ref })?;
    Ok(Html(rendered))
}

/// Check visitor requirements and submit an application.
async fn page_visitor_application(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let controller: Option<Controller> = match user_info {
        Some(ref info) => {
            let controller: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
                .bind(info.cid)
                .fetch_optional(&state.db)
                .await?;
            controller
        }
        None => None,
    };
    let is_visiting = controller
        .as_ref()
        .map(|c| c.is_on_roster)
        .unwrap_or_default();
    if let Some(ref ui) = user_info {
        info!("{} accessed visitor application page", ui.cid);
    }
    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let template = state
        .templates
        .get_template("facility/visitor_application.jinja")?;
    let rendered =
        template.render(context! { user_info, flashed_messages, controller, is_visiting })?;
    Ok(Html(rendered))
}

/// Check visitor eligibility and return either a form or an error message.
async fn page_visitor_application_form(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    let user_info: UserInfo = match session.get(SESSION_USER_INFO_KEY).await? {
        Some(user_info) => user_info,
        // a little lazy, but no one should see this
        None => return Ok(Html(String::from("Must be logged in"))),
    };
    info!("{} accessing visitor application form", user_info.cid);

    let template = state
        .templates
        .get_template("facility/visitor_application_form.jinja")?;

    // check pending request
    let pending_request: Option<VisitorRequest> = sqlx::query_as(sql::GET_PENDING_VISITOR_REQ_FOR)
        .bind(user_info.cid)
        .fetch_optional(&state.db)
        .await?;
    if pending_request.is_some() {
        info!(
            "{} already has a pending visitor application",
            user_info.cid
        );
        let rendered = template.render(context! { user_info, pending_request })?;
        return Ok(Html(rendered));
    }

    // get controller info
    let controller_info = match vatusa::get_controller_info(user_info.cid, None).await {
        Ok(info) => info,
        Err(e) => {
            error!("Error getting controller info from VATUSA: {e}");
            let rendered = template.render(
                context! { user_info, error => "Could not get controller info from VATUSA" },
            )?;
            return Ok(Html(rendered));
        }
    };

    // check VATUSA checklist
    let checklist = match vatusa::transfer_checklist(
        user_info.cid,
        &state.config.vatsim.vatusa_api_key,
    )
    .await
    {
        Ok(checklist) => checklist,
        Err(e) => {
            error!("Error getting transfer checklist from VATUSA: {e}");
            let rendered = template.render(
                context! { user_info, error => "Could not get controller visit/transfer checklist info from VATUSA" },
            )?;
            return Ok(Html(rendered));
        }
    };

    info!(
        "Rendering visitor app form for {}; visiting: {}, rating: {}, rating_90_days: {}, controlled_50_hours: {}, last_visit_60_days: {}",
        user_info.cid,
        checklist.visiting,
        controller_info.rating,
        checklist.rating_90_days,
        checklist.controlled_50_hrs,
        checklist.last_visit_60_days
    );
    let rendered =
        template.render(context! { user_info, pending_request, controller_info, checklist })?;
    Ok(Html(rendered))
}

#[derive(Debug, Deserialize)]
struct VisitorApplicationForm {
    rating: u8,
    facility: String,
}

/// Submit the request to join as a visitor.
async fn page_visitor_application_form_submit(
    State(state): State<Arc<AppState>>,
    session: Session,
    Form(application_form): Form<VisitorApplicationForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let user_info = match user_info {
        Some(ui) => ui,
        None => {
            flashed_messages::push_flashed_message(
                session,
                MessageLevel::Error,
                "You must be logged in to submit a visitor request.",
            )
            .await?;
            return Ok(Redirect::to("/"));
        }
    };

    sqlx::query(sql::INSERT_INTO_VISITOR_REQ)
        .bind(user_info.cid)
        .bind(&user_info.first_name)
        .bind(&user_info.last_name)
        .bind(&application_form.facility)
        .bind(application_form.rating)
        .bind(Utc::now())
        .execute(&state.db)
        .await?;
    flashed_messages::push_flashed_message(
        session,
        MessageLevel::Success,
        "Request submitted, thank you!",
    )
    .await?;
    let notification_webhook = state.config.discord.webhooks.new_visitor_app.clone();
    let notification_content = format!(
        "New visitor app from {} {}, CID {}, rating {}, visiting from {}",
        user_info.first_name,
        user_info.last_name,
        user_info.cid,
        ControllerRating::try_from(application_form.rating as i8)
            .map(|cr| cr.as_str())
            .unwrap_or("?"),
        application_form.facility
    );
    tokio::spawn(async move {
        let res = GENERAL_HTTP_CLIENT
            .post(&notification_webhook)
            .json(&json!({ "content": notification_content }))
            .send()
            .await;
        if let Err(e) = res {
            error!("Could not send info to new visitor app webhook: {e}");
        }
    });
    Ok(Redirect::to("/facility/visitor_application"))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/facility/roster", get(page_roster))
        .route("/facility/roster/stats", get(page_roster_stats))
        .route("/facility/roster/stats/data", get(api_roster_stats))
        .route("/facility/staff", get(page_staff))
        .route("/facility/activity", get(page_activity))
        .route(
            "/facility/resources",
            get(page_resources).post(post_page_resources_sign),
        )
        .route("/facility/aliasref", get(alias_ref))
        .route(
            "/facility/visitor_application",
            get(page_visitor_application),
        )
        .route(
            "/facility/visitor_application/form",
            get(page_visitor_application_form).post(page_visitor_application_form_submit),
        )
}
