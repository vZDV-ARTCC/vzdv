//! vZDV website

#![deny(clippy::all)]
#![allow(clippy::collapsible_if)]
#![deny(unsafe_code)]

use axum::{Router, middleware as axum_middleware};
use chrono::{Datelike, Days, NaiveDate, Utc, Weekday};
use chrono_tz::America::Denver;
use clap::Parser;
use log::{debug, error, info, warn};
use mini_moka::sync::Cache;
use minijinja::Environment;
use shared::{AppError, AppState, ERROR_WEBHOOK};
use std::{
    fs,
    path::{Path, PathBuf},
    process,
    sync::Arc,
    time::Duration,
};
use thousands::Separable;
use tokio::signal;
use tower::ServiceBuilder;
use tower_http::timeout::TimeoutLayer;
use tower_sessions::{Expiry, SessionManagerLayer};
use tower_sessions_sqlx_store::SqliteStore;
use vzdv::{ControllerRating, general_setup, splits::SectorsConfig};

mod discord;
mod endpoints;
mod flashed_messages;
mod flights;
mod middleware;
mod shared;
mod vatusa;

/// vZDV website.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Load the config from a specific file.
    ///
    /// [default: vzdv.toml]
    #[arg(long)]
    config: Option<PathBuf>,

    /// Enable debug logging
    #[arg(short, long)]
    debug: bool,

    /// Host to run on
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    /// Port to run on
    #[arg(long, default_value_t = 3000)]
    port: u16,
}

/// The seasonal theme to show on a date, if any. Each theme is a
/// stylesheet in `static/holiday/`, shown on its first and last days.
fn holiday_on(date: NaiveDate) -> Option<&'static str> {
    let year = date.year();
    let day = |month, day| NaiveDate::from_ymd_opt(year, month, day).unwrap();
    // the fourth Thursday of November
    let thanksgiving = NaiveDate::from_weekday_of_month_opt(year, 11, Weekday::Thu, 4).unwrap();
    [
        ("halloween", day(10, 1), day(10, 31)),
        // the Monday before through the Friday after
        (
            "thanksgiving",
            thanksgiving - Days::new(3),
            thanksgiving + Days::new(1),
        ),
        ("christmas", day(12, 1), day(12, 31)),
    ]
    .into_iter()
    .find(|(_, first, last)| (*first..=*last).contains(&date))
    .map(|(name, _, _)| name)
}

/// Load all template files into the binary via the stdlib `include_str!`
/// macro and supply to the minijinja environment.
fn load_templates() -> Result<Environment<'static>, AppError> {
    let mut env = Environment::new();

    #[cfg(feature = "bundled")]
    {
        minijinja_embed::load_templates!(&mut env);
    }

    #[cfg(not(feature = "bundled"))]
    {
        env.set_loader(minijinja::path_loader("vzdv-site/templates"));
    }

    env.add_filter("minutes_to_hm", |total_minutes: u32| {
        if total_minutes == 0 {
            return String::new();
        }
        let hours = total_minutes / 60;
        let minutes = total_minutes % 60;
        if hours > 0 || minutes > 0 {
            format!("{hours}h{minutes}m")
        } else {
            String::new()
        }
    });
    env.add_filter("simple_date", |date: String| {
        chrono::DateTime::parse_from_rfc3339(&date)
            .unwrap()
            .format("%m/%d/%Y")
            .to_string()
    });
    env.add_function(
        "includes",
        |roles: Vec<String>, role: String| -> Result<bool, minijinja::Error> {
            Ok(roles.contains(&role))
        },
    );
    env.add_filter("format_number", |value: u16| value.separate_with_commas());
    env.add_filter("nice_date", |date: String| {
        chrono::DateTime::parse_from_rfc3339(&date)
            .unwrap()
            .format("%m/%d/%Y %H:%M:%S")
            .to_string()
    });
    env.add_filter(
        "rating_str",
        |rating: i8| match ControllerRating::try_from(rating) {
            Ok(r) => r.as_str(),
            Err(_) => "OBS",
        },
    );
    env.add_filter("capitalize_first", |s: String| {
        let mut c = s.chars();
        match c.next() {
            None => String::new(),
            Some(f) => f.to_uppercase().chain(c).collect(),
        }
    });
    env.add_filter("unix_timestamp", |date: String| {
        chrono::DateTime::parse_from_rfc3339(&date)
            .unwrap()
            .timestamp()
            .to_string()
    });
    env.add_filter("urlencode", |s: String| urlencoding::encode(&s).to_string());
    env.add_function("active_holiday", || {
        holiday_on(Utc::now().with_timezone(&Denver).date_naive())
    });

    Ok(env)
}

/// Create all the endpoints and insert middleware.
fn load_router(
    sessions_layer: SessionManagerLayer<SqliteStore>,
    app_state: &Arc<AppState>,
) -> Router<Arc<AppState>> {
    Router::new()
        .merge(endpoints::router(app_state))
        .merge(endpoints::admin::router())
        .merge(endpoints::api::router())
        .merge(endpoints::airspace::router())
        .merge(endpoints::auth::router())
        .merge(endpoints::controller::router())
        .merge(endpoints::events::router())
        .merge(endpoints::facility::router())
        .merge(endpoints::homepage::router())
        .merge(endpoints::ids::router())
        .merge(endpoints::user::router())
        .layer(
            ServiceBuilder::new()
                .layer(TimeoutLayer::new(Duration::from_secs(30)))
                .layer(axum_middleware::from_fn(middleware::logging))
                .layer(sessions_layer)
                .layer(axum_middleware::from_fn(middleware::extend_session)),
        )
        .fallback(endpoints::page_404)
}

// https://github.com/tokio-rs/axum/blob/main/examples/graceful-shutdown/src/main.rs
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
        warn!("Got terminate signal");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
        warn!("Got terminate signal");
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// Entrypoint.
#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let (config, db) = general_setup(cli.debug, "vzdv_site", cli.config).await;
    ERROR_WEBHOOK
        .set(config.discord.webhooks.errors.clone())
        .expect("Could not set global error webhook");

    let sessions = SqliteStore::new(db.clone());
    if let Err(e) = sessions.migrate().await {
        error!("Could not create table for sessions: {e}");
        return;
    }

    // "lax" seems to be needed for the Discord OAuth login, but is there a concern about security?
    let session_layer = SessionManagerLayer::new(sessions)
        .with_same_site(tower_sessions::cookie::SameSite::Lax)
        .with_expiry(Expiry::OnInactivity(time::Duration::hours(
            middleware::SESSION_INACTIVITY_WINDOW,
        )));
    let templates = match load_templates() {
        Ok(t) => t,
        Err(e) => {
            error!("Could not load the first templates: {e}");
            return;
        }
    };
    debug!("Loaded");

    debug!("Loading sector configuration");
    let sectors_config = match fs::read_to_string("splits.json") {
        Ok(text) => match serde_json::from_str::<SectorsConfig>(&text) {
            Ok(s) => s,
            Err(e) => {
                error!("Could not parse splits.json: {e}");
                return;
            }
        },
        Err(e) => {
            error!("Could not read splits.json: {e}");
            return;
        }
    };

    debug!("Setting up app");
    let app_state = Arc::new(AppState {
        config,
        sectors_config,
        db: db.clone(),
        templates,
        cache: Cache::new(30),
    });
    let router = load_router(session_layer, &app_state);
    let app = router.with_state(app_state);
    let assets_dir = Path::new("./assets");
    if !assets_dir.exists() {
        if let Err(e) = fs::create_dir(assets_dir) {
            error!("Could not create assets directory: {e}");
            process::exit(1);
        }
        debug!("Assets directory created");
    }
    debug!("Set up");

    let host_and_port = format!("{}:{}", cli.host, cli.port);
    info!("Listening on http://{host_and_port}/");
    let listener = tokio::net::TcpListener::bind(&host_and_port)
        .await
        .expect("Could not bind the HTTP listener");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("Could not serve the app");
    db.close().await;
}

#[cfg(test)]
mod tests {
    use super::holiday_on;
    use chrono::NaiveDate;

    #[test]
    fn test_holiday_on() {
        let on = |year, month, day| holiday_on(NaiveDate::from_ymd_opt(year, month, day).unwrap());
        assert_eq!(on(2026, 9, 30), None);
        assert_eq!(on(2026, 10, 1), Some("halloween"));
        assert_eq!(on(2026, 10, 31), Some("halloween"));
        assert_eq!(on(2026, 11, 1), None);

        // Thanksgiving 2026 is November 26th, and 2027 is November 25th
        assert_eq!(on(2026, 11, 22), None);
        assert_eq!(on(2026, 11, 23), Some("thanksgiving"));
        assert_eq!(on(2026, 11, 27), Some("thanksgiving"));
        assert_eq!(on(2026, 11, 28), None);
        assert_eq!(on(2027, 11, 21), None);
        assert_eq!(on(2027, 11, 22), Some("thanksgiving"));
        assert_eq!(on(2027, 11, 26), Some("thanksgiving"));
        assert_eq!(on(2027, 11, 27), None);

        assert_eq!(on(2026, 11, 30), None);
        assert_eq!(on(2026, 12, 1), Some("christmas"));
        assert_eq!(on(2026, 12, 31), Some("christmas"));
        assert_eq!(on(2027, 1, 1), None);
    }
}
