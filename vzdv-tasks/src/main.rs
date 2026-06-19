//! vZDV website background task runner.

#![deny(clippy::all)]
#![deny(unsafe_code)]

use clap::Parser;
use clokwerk::{AsyncScheduler, TimeUnits};
use log::{debug, error, info};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use vzdv::general_setup;

mod activity;
mod atis;
mod currency;
mod no_show_expiration;
mod roster;
mod solo_cert;
mod traffic_tracking;

/// vZDV task runner.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Load the config from a specific file.
    ///
    /// [default: vzdv.toml]
    #[arg(long)]
    config: Option<PathBuf>,

    #[arg(long)]
    ids_config: Option<PathBuf>,

    /// Enable debug logging
    #[arg(short, long)]
    debug: bool,
}

/// Entrypoint.
#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let (config, db, _) = general_setup(cli.debug, "vzdv_tasks", cli.config, cli.ids_config).await;
    let config = Arc::new(config);
    let db = Arc::new(db);
    let mut scheduler = AsyncScheduler::with_tz(chrono::Utc);

    /*
     * Semaphores are used to prevent partial updating of roster and
     * activity to happen at the same time as the updating of the
     * *full* roster and activity. In the previous design, this wasn't
     * needed as the partial and full updates were never performed
     * at the same time, but they can with this scheduler, so a
     * mechanism is needed to ensure that the two async tasks
     * don't collide with each other.
     */

    let roster_semaphore = Arc::new(Semaphore::new(1));
    let activity_semaphore = Arc::new(Semaphore::new(1));

    // every 5 minutes, partial roster update
    {
        let db = Arc::clone(&db);
        let roster_semaphore = Arc::clone(&roster_semaphore);
        scheduler.every(5.minutes()).run(move || {
            let db = Arc::clone(&db);
            let roster_semaphore = Arc::clone(&roster_semaphore);
            async move {
                debug!("Partial roster update tick");
                // don't try to do a partial update when the full update is processing
                if roster_semaphore.try_acquire().is_err() {
                    return;
                }
                if let Err(e) = roster::partial_update_roster(&db).await {
                    error!("Error partial updating roster: {e}")
                }
            }
        });
    }

    // every 2 hours, full roster update
    {
        let db = Arc::clone(&db);
        let roster_semaphore = Arc::clone(&roster_semaphore);
        scheduler.every(2.hours()).run(move || {
            let db = Arc::clone(&db);
            let roster_semaphore = Arc::clone(&roster_semaphore);
            async move {
                debug!("Full roster update tick");
                // lock the semaphore while updating the whole roster
                let _ = roster_semaphore.acquire().await.unwrap();
                info!("Querying roster");
                match roster::update_roster(&db).await {
                    Ok(_) => info!("Roster update successful"),
                    Err(e) => error!("Error updating roster: {e}"),
                }
            }
        });
    }

    // every 15 minutes, partial activity check
    {
        let db = Arc::clone(&db);
        let config = Arc::clone(&config);
        let activity_semaphore = Arc::clone(&activity_semaphore);
        scheduler.every(15.minutes()).run(move || {
            let db = Arc::clone(&db);
            let config = Arc::clone(&config);
            let activity_semaphore = Arc::clone(&activity_semaphore);
            async move {
                debug!("Partial activity sync tick");
                // don't try to do a partial update when the full update is processing
                if activity_semaphore.try_acquire().is_err() {
                    return;
                }
                if let Err(e) = activity::update_online_controller_activity(&config, &db).await {
                    error!("Error updating partial activity: {e}");
                }
            }
        });
    }

    // every 6 hours, full activity sync
    {
        let db = Arc::clone(&db);
        let config = Arc::clone(&config);
        let roster_semaphore = Arc::clone(&roster_semaphore);
        scheduler.every(6.hours()).run(move || {
            let db = Arc::clone(&db);
            let config = Arc::clone(&config);
            let roster_semaphore = Arc::clone(&roster_semaphore);
            async move {
                debug!("Full activity sync tick");
                // lock the semaphore while updating the whole roster
                let _ = roster_semaphore.acquire().await.unwrap();
                info!("Updating all activity");
                match activity::true_up_all_controllers_activity(&config, &db).await {
                    Ok(_) => info!("Full activity update successful"),
                    Err(e) => error!("Error updating full activity: {e}"),
                }
            }
        });
    }

    // every 30 minutes, solo cert expiration check
    {
        let db = Arc::clone(&db);
        scheduler.every(30.minutes()).run(move || {
            let db = Arc::clone(&db);
            async move {
                debug!("Solo cert expiration tick");
                if let Err(e) = solo_cert::check_expired(&db).await {
                    error!("Error checking for solo cert expiration: {e}");
                }
            }
        });
    }

    // every 12 hours, no show expiration check
    {
        let db = Arc::clone(&db);
        scheduler.every(12.hours()).run(move || {
            let db = Arc::clone(&db);
            async move {
                if let Err(e) = no_show_expiration::check_expired(&db).await {
                    error!("Error checking for no-show expiration: {e}");
                }
            }
        });
    }

    // every 5 minutes, clean up ATIS data
    {
        let db = Arc::clone(&db);
        scheduler.every(5.minutes()).run(move || {
            let db = Arc::clone(&db);
            async move {
                debug!("ATIS cleanup tick");
                if let Err(e) = atis::cleanup(&db).await {
                    error!("Error cleaning up ATIS data: {e}");
                }
            }
        });
    }

    // every 24 hours, check if activity reminders need to be sent out
    {
        let db = Arc::clone(&db);
        let config = Arc::clone(&config);
        scheduler.every(24.hours()).run(move || {
            let db = Arc::clone(&db);
            let config = Arc::clone(&config);
            async move {
                debug!("Activity warning check tick");
                if let Err(e) = currency::reminders(&config, &db).await {
                    error!("Error sending currency reminder emails: {e}");
                }
            }
        });
    }

    info!("Waiting 5 seconds before kicking off the scheduler");
    tokio::time::sleep(Duration::from_secs(5)).await;
    info!("Starting scheduler");

    // poll the scheduler
    loop {
        scheduler.run_pending().await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
