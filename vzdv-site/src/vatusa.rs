//! Wrappers around the `vzdv::vatusa` module using `AppError`.

use crate::AppError;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Serialize;
pub use vzdv::vatusa::{NewTrainingRecord, TrainingRecord};
use vzdv::{
    sql::AuxiliaryTrainingData,
    vatusa::{self, RatingHistory, RosterMember, TransferChecklist, VatusaError},
};

/// Map the generic `anyhow::Error` from the `vzdv::vatusa` module function
/// to an `AppError` for this crate.
///
/// All errors from the `vzdv::vatusa` module _should_ be of the
/// `vzdv::vatusa::VatusaError` type, which gives additional information
/// about the source and reason for the error.
fn map_err(error: anyhow::Error) -> AppError {
    if let Some(e) = error.downcast_ref::<VatusaError>() {
        AppError::VatusaApi(e.to_owned())
    } else {
        // not sure when this branch would be reached
        AppError::GenericFallback("accessing VATUSA API", error)
    }
}

/// Get the controller's public information.
///
/// Supply a VATUSA API key to get private information.
pub async fn get_controller_info(
    cid: u32,
    api_key: Option<&str>,
) -> Result<RosterMember, AppError> {
    let data = vatusa::get_controller_info(cid, api_key)
        .await
        .map_err(map_err)?;
    Ok(data)
}

/// Get the controller's training records.
pub async fn get_training_records(
    cid: u32,
    api_key: &str,
) -> Result<Vec<TrainingRecord>, AppError> {
    let data = vatusa::get_training_records(cid, api_key)
        .await
        .map_err(map_err)?;
    Ok(data)
}

/// Add a new training record to the controller's VATUSA record.
pub async fn save_training_record(
    api_key: &str,
    cid: u32,
    data: &NewTrainingRecord,
) -> Result<(), AppError> {
    vatusa::save_training_record(api_key, cid, data)
        .await
        .map_err(map_err)?;
    Ok(())
}

/// Get the controller's transfer checklist information.
pub async fn transfer_checklist(cid: u32, api_key: &str) -> Result<TransferChecklist, AppError> {
    let data = vatusa::transfer_checklist(cid, api_key)
        .await
        .map_err(map_err)?;
    Ok(data)
}

/// Report a new solo cert to VATUSA.
pub async fn report_solo_cert(
    cid: u32,
    position: &str,
    expiration: DateTime<Utc>,
    api_key: &str,
) -> Result<(), AppError> {
    vatusa::report_solo_cert(cid, position, expiration, api_key)
        .await
        .map_err(map_err)?;
    Ok(())
}

/// Delete a solo cert from VATUSA.
pub async fn delete_solo_cert(cid: u32, position: &str, api_key: &str) -> Result<(), AppError> {
    vatusa::delete_solo_cert(cid, position, api_key)
        .await
        .map_err(map_err)?;
    Ok(())
}

/// Get a controller's rating history.
pub async fn get_controller_rating_history(
    cid: u32,
    api_key: &str,
) -> Result<Vec<RatingHistory>, AppError> {
    let data = vatusa::get_controller_rating_history(cid, api_key)
        .await
        .map_err(map_err)?;
    Ok(data)
}

/// Remove a home controller from the roster.
pub async fn remove_home_controller(
    cid: u32,
    by: &str,
    reason: &str,
    api_key: &str,
) -> Result<(), AppError> {
    vatusa::remove_home_controller(cid, by, reason, api_key)
        .await
        .map_err(map_err)?;
    Ok(())
}

/// Remove a visiting controller from the roster.
pub async fn remove_visiting_controller(
    cid: u32,
    reason: &str,
    api_key: &str,
) -> Result<(), AppError> {
    vatusa::remove_visiting_controller(cid, reason, api_key)
        .await
        .map_err(map_err)?;
    Ok(())
}

/// Add a visiting controller to the roster.
pub async fn add_visiting_controller(cid: u32, api_key: &str) -> Result<(), AppError> {
    vatusa::add_visiting_controller(cid, api_key)
        .await
        .map_err(map_err)?;
    Ok(())
}

/// Combination of VATUSA training records and database auxiliary records.
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum TrainingDataType {
    VatusaRecord(TrainingRecord),
    AuxData(AuxiliaryTrainingData),
}

impl TrainingDataType {
    pub fn get_date(&self) -> DateTime<Utc> {
        match self {
            TrainingDataType::AuxData(record) => record.session_date,
            TrainingDataType::VatusaRecord(record) => {
                let dt = NaiveDateTime::parse_from_str(&record.session_date, "%Y-%m-%d %H:%M:%S")
                    .unwrap_or_default();
                DateTime::from_naive_utc_and_offset(dt, Utc)
            }
        }
    }

    pub fn trainer(&self) -> u32 {
        match self {
            TrainingDataType::VatusaRecord(record) => record.instructor_id,
            TrainingDataType::AuxData(record) => record.trainer,
        }
    }
}
