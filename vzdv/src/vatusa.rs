use crate::GENERAL_HTTP_CLIENT;
use anyhow::Result;
use chrono::{DateTime, NaiveDateTime, Utc};
use log::warn;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use tokio::task::JoinSet;

const BASE_URL: &str = "https://api.vatusa.net/";

#[derive(Debug, thiserror::Error, Clone)]
pub enum VatusaError {
    #[error("A {0} error was returned by the VATUSA {1} API: {2}")]
    Reason(u16, &'static str, String),
    #[error("A {0} unknown error was returned by the VATUSA {1} API")]
    Unknown(u16, &'static str),
}

pub enum MembershipType {
    Home,
    Visit,
    Both,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RosterMemberRole {
    pub id: u32,
    pub cid: u32,
    pub facility: String,
    pub role: String,
    pub created_at: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RosterMemberVisiting {
    pub id: u32,
    pub cid: u32,
    pub facility: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RosterMember {
    pub cid: u32,
    #[serde(rename = "fname")]
    pub first_name: String,
    #[serde(rename = "lname")]
    pub last_name: String,
    pub email: Option<String>,
    pub facility: String,
    pub rating: i8,
    pub created_at: String,
    pub updated_at: String,
    #[serde(rename = "flag_needbasic")]
    pub flag_need_basic: bool,
    #[serde(rename = "flag_xferOverride")]
    pub flag_transfer_override: bool,
    pub facility_join: String,
    #[serde(rename = "flag_homecontroller")]
    pub flag_home_controller: bool,
    #[serde(rename = "lastactivity")]
    pub last_activity: String,
    #[serde(rename = "flag_broadcastOptedIn")]
    pub flag_broadcast_opted_in: Option<bool>,
    #[serde(rename = "flag_preventStaffAssign")]
    pub flag_prevent_staff_assign: Option<bool>,
    pub discord_id: Option<u64>,
    pub last_cert_sync: String,
    #[serde(rename = "flag_nameprivacy")]
    pub flag_name_privacy: bool,
    pub last_competency_date: Option<String>,
    pub promotion_eligible: Option<bool>,
    pub transfer_eligible: Option<serde_json::Value>,
    pub roles: Vec<RosterMemberRole>,
    pub rating_short: String,
    pub visiting_facilities: Option<Vec<RosterMemberVisiting>>,
    #[serde(rename = "isMentor")]
    pub is_mentor: bool,
    #[serde(rename = "isSupIns")]
    pub is_sup_ins: bool,
    pub last_promotion: Option<String>,
}

/// Get the roster of a VATUSA facility.
pub async fn get_roster(facility: &str, membership: MembershipType) -> Result<Vec<RosterMember>> {
    #[derive(Deserialize)]
    pub struct Wrapper {
        pub data: Vec<RosterMember>,
    }

    let mem_str = match membership {
        MembershipType::Home => "home",
        MembershipType::Visit => "visit",
        MembershipType::Both => "both",
    };
    let resp = GENERAL_HTTP_CLIENT
        .get(format!("{BASE_URL}facility/{facility}/roster/{mem_str}"))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(VatusaError::Unknown(resp.status().as_u16(), "roster").into());
    }
    let data: Wrapper = resp.json().await?;
    Ok(data.data)
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TransferChecklist {
    #[serde(rename = "homecontroller")]
    pub home_controller: bool,
    #[serde(rename = "needbasic")]
    pub need_basic: bool,
    pub pending: bool,
    pub initial: bool,
    #[serde(rename = "90days")]
    pub rating_90_days: bool,
    pub promo: bool,
    #[serde(rename = "50hrs")]
    pub controlled_50_hrs: bool,
    #[serde(rename = "override")]
    pub has_override: bool,
    pub is_first: u8,
    pub days: u32,
    #[serde(rename = "visitingDays")]
    pub visiting_days: Option<u32>,
    #[serde(rename = "60days")]
    pub last_visit_60_days: bool,
    #[serde(rename = "promoDays")]
    promo_days: Option<u32>,
    #[serde(rename = "ratingHours")]
    rating_hours: Option<f32>,
    #[serde(rename = "hasHome")]
    pub has_home: bool,
    #[serde(rename = "hasRating")]
    pub has_rating: bool,
    pub instructor: bool,
    pub staff: bool,
    /// Computed flag for whether or not the controller meets basic visiting requirements
    pub visiting: bool,
    pub overall: bool,
}

/// Get the controller's transfer checklist information.
pub async fn transfer_checklist(cid: u32, api_key: &str) -> Result<TransferChecklist> {
    #[derive(Deserialize)]
    pub struct Wrapper {
        pub data: TransferChecklist,
    }

    let resp = GENERAL_HTTP_CLIENT
        .get(format!("{BASE_URL}v2/user/{cid}/transfer/checklist"))
        .query(&[("apikey", api_key)])
        .send()
        .await?;
    if !resp.status().is_success() {
        warn!("Transfer checklist error for {cid}");
        return Err(VatusaError::Unknown(resp.status().as_u16(), "transfer checklist").into());
    }
    let data: Wrapper = resp.json().await?;
    Ok(data.data)
}

/// Get the controller's public information.
///
/// Supply a VATUSA API key to get private information.
pub async fn get_controller_info(cid: u32, api_key: Option<&str>) -> Result<RosterMember> {
    #[derive(Deserialize)]
    pub struct Wrapper {
        pub data: RosterMember,
    }

    let mut req = GENERAL_HTTP_CLIENT.get(format!("{BASE_URL}user/{cid}"));
    if let Some(key) = api_key {
        req = req.query(&[("apikey", key)]);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        return Err(VatusaError::Unknown(resp.status().as_u16(), "controller info").into());
    }
    let data: Wrapper = resp.json().await?;
    Ok(data.data)
}

/// Get multiple controller info documents at the same time.
///
/// Instead of returning errors, this function simply omits info
/// from any request that failed.
pub async fn get_multiple_controller_info(cids: &[u32]) -> Vec<RosterMember> {
    let mut set = JoinSet::new();
    for &cid in cids {
        set.spawn(async move { get_controller_info(cid, None).await });
    }
    let mut info = Vec::new();
    while let Some(res) = set.join_next().await {
        if let Ok(Ok(data)) = res {
            info.push(data);
        }
    }
    info
}

/// Retrieve multiple controller first and last names from the API by CIDs.
///
/// Any network calls that fail are simply not included in the returned map.
pub async fn get_multiple_controller_names(cids: &[u32]) -> HashMap<u32, String> {
    let info = get_multiple_controller_info(cids).await;
    info.iter().fold(HashMap::new(), |mut map, info| {
        map.insert(info.cid, format!("{} {}", info.first_name, info.last_name));
        map
    })
}

/// Add a visiting controller to the roster.
pub async fn add_visiting_controller(cid: u32, api_key: &str) -> Result<()> {
    let resp = GENERAL_HTTP_CLIENT
        .post(format!(
            "{BASE_URL}v2/facility/ZDV/roster/manageVisitor/{cid}"
        ))
        .query(&[("apikey", api_key)])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(VatusaError::Unknown(resp.status().as_u16(), "visitor add").into());
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TrainingRecord {
    pub id: u32,
    pub student_id: u32,
    pub instructor_id: u32,
    pub session_date: String,
    pub facility_id: String,
    pub position: String,
    pub duration: String,
    pub notes: String,
}

/// Get the controller's training records.
pub async fn get_training_records(cid: u32, api_key: &str) -> Result<Vec<TrainingRecord>> {
    #[derive(Deserialize)]
    pub struct Wrapper {
        pub data: Vec<TrainingRecord>,
    }

    let resp = GENERAL_HTTP_CLIENT
        .get(format!("{BASE_URL}v2/user/{cid}/training/records"))
        .query(&[("apikey", api_key)])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(VatusaError::Unknown(resp.status().as_u16(), "get training records").into());
    }
    let data: Wrapper = resp.json().await?;
    Ok(data.data)
}

/// VATUSA training record "location" values.
pub mod training_record_location {
    pub const CLASSROOM: u8 = 0;
    pub const LIVE: u8 = 0;
    pub const SIMULATION: u8 = 2;
    pub const LIVE_OTS: u8 = 1;
    pub const SIMULATION_OTS: u8 = 2;
    pub const NO_SHOW: u8 = 0;
    pub const OTHER: u8 = 0;
}

/// Data required for creating a new VATUSA training record.
///
/// The CID must also be supplied.
#[derive(Debug, Deserialize, Serialize)]
pub struct NewTrainingRecord {
    pub instructor_id: String,
    pub date: NaiveDateTime,
    pub position: String,
    pub duration: String,
    pub location: u8,
    pub notes: String,
}

#[derive(Debug, Deserialize)]
pub struct ErrorRespData {
    #[serde(rename = "msg")]
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct ErrorResp {
    pub data: ErrorRespData,
}

/// Add a new training record to the controller's VATUSA record.
pub async fn save_training_record(api_key: &str, cid: u32, data: &NewTrainingRecord) -> Result<()> {
    let resp = GENERAL_HTTP_CLIENT
        .post(format!("{BASE_URL}v2/user/{cid}/training/record"))
        .query(&[("apikey", api_key)])
        .json(&json!({
            "instructor_id": data.instructor_id,
            "session_date": data.date.format("%Y-%m-%d %H:%M").to_string(),
            "position": data.position,
            "duration": &data.duration,
            "location": data.location,
            "notes": data.notes
        }))
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let reason = if let Ok(error_body) = resp.json::<ErrorResp>().await {
            error_body.data.message
        } else {
            "unknown".to_string()
        };
        return Err(VatusaError::Reason(status, "training record submission", reason).into());
    }
    Ok(())
}

/// Remove a home controller from the roster.
pub async fn remove_home_controller(cid: u32, by: &str, reason: &str, api_key: &str) -> Result<()> {
    let resp = GENERAL_HTTP_CLIENT
        .delete(format!("{BASE_URL}v2/facility/ZDV/roster/{cid}"))
        .query(&[("apikey", api_key)])
        .json(&json!({ "by": by, "reason": reason }))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(VatusaError::Unknown(resp.status().as_u16(), "home controller removal").into());
    }
    Ok(())
}

/// Remove a visiting controller from the roster.
pub async fn remove_visiting_controller(cid: u32, reason: &str, api_key: &str) -> Result<()> {
    let resp = GENERAL_HTTP_CLIENT
        .delete(format!(
            "{BASE_URL}v2/facility/ZDV/roster/manageVisitor/{cid}"
        ))
        .query(&[("apikey", api_key)])
        .json(&json!({ "reason": reason }))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(
            VatusaError::Unknown(resp.status().as_u16(), "visiting controller removal").into(),
        );
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RatingHistory {
    pub id: u32,
    pub cid: u32,
    pub grantor: u32,
    pub to: i8,
    pub from: i8,
    pub created_at: String,
    pub exam: String,
    pub examiner: u32,
    pub position: String,
    pub eval_id: Option<serde_json::Value>,
}

/// Get a controller's rating history.
pub async fn get_controller_rating_history(cid: u32, api_key: &str) -> Result<Vec<RatingHistory>> {
    #[derive(Deserialize)]
    pub struct Wrapper {
        pub data: Vec<RatingHistory>,
    }

    let resp = GENERAL_HTTP_CLIENT
        .get(format!("{BASE_URL}user/{cid}/rating/history"))
        .query(&[("apikey", api_key)])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(
            VatusaError::Unknown(resp.status().as_u16(), "controller rating history").into(),
        );
    }
    let data: Wrapper = resp.json().await?;
    Ok(data.data)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SoloCertification {
    pub id: u32,
    pub cid: u32,
    pub position: String,
    pub expires: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Get a list of solo certifications from VATUSA for the facility.
pub async fn get_facility_solo_certs() -> Result<Vec<SoloCertification>> {
    #[derive(Deserialize)]
    pub struct Wrapper {
        pub data: Vec<SoloCertification>,
    }

    let resp = GENERAL_HTTP_CLIENT
        .get(format!("{BASE_URL}solo"))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(VatusaError::Unknown(resp.status().as_u16(), "facility solo certs").into());
    }
    let data: Wrapper = resp.json().await?;
    Ok(data.data)
}

/// Report a new solo cert to VATUSA.
pub async fn report_solo_cert(
    cid: u32,
    position: &str,
    expiration: DateTime<Utc>,
    api_key: &str,
) -> Result<()> {
    let data = HashMap::from([
        ("cid", cid.to_string()),
        ("position", position.to_owned()),
        ("expDate", expiration.format("%Y-%m-%d").to_string()),
    ]);
    let resp = GENERAL_HTTP_CLIENT
        .post(format!("{BASE_URL}solo"))
        .query(&[("apikey", api_key)])
        .form(&data)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(VatusaError::Unknown(resp.status().as_u16(), "add solo cert").into());
    }
    Ok(())
}

/// Delete a solo cert from VATUSA.
pub async fn delete_solo_cert(cid: u32, position: &str, api_key: &str) -> Result<()> {
    let data = HashMap::from([("cid", cid.to_string()), ("position", position.to_owned())]);
    let resp = GENERAL_HTTP_CLIENT
        .delete(format!("{BASE_URL}solo"))
        .query(&[("apikey", api_key)])
        .form(&data)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(VatusaError::Unknown(resp.status().as_u16(), "delete solo cert").into());
    }
    Ok(())
}
