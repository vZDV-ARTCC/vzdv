#![allow(dead_code)]

use anyhow::{Result, bail};
use sqlx::{Pool, Sqlite};
use tokio::sync::OnceCell;
use vatsim_utils::{errors::VatsimUtilError, live_api::Vatsim};
use vzdv::sql;

static VATSIM: OnceCell<std::result::Result<Vatsim, VatsimUtilError>> = OnceCell::const_new();

/// Retrieve the live data from VATSIM and store in the database.
pub async fn store_live_data(db: &Pool<Sqlite>) -> Result<()> {
    let vatsim = match VATSIM.get_or_init(|| async { Vatsim::new().await }).await {
        Ok(v) => v,
        Err(e) => {
            bail!("could not get VATSIM struct init: {e}");
        }
    };

    let data = vatsim.get_v3_data().await?;
    let text = serde_json::to_string(&data)?;
    sqlx::query(sql::UPSERT_KVS_ENTRY)
        .bind("live-data")
        .bind(&text)
        .execute(db)
        .await?;

    Ok(())
}
