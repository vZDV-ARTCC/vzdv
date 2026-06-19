//! vZDV Discord bot.

#![deny(clippy::all)]
#![allow(clippy::collapsible_if)]
#![deny(unsafe_code)]

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose};
use clap::Parser;
use log::{debug, error, info, warn};
use sqlx::{Pool, Sqlite};
use std::{path::PathBuf, sync::Arc};
use twilight_gateway::{Event, Intents, Shard, ShardId};
use twilight_http::Client as HttpClient;
use twilight_interactions::command::CreateCommand;
use twilight_model::id::Id;
use vzdv::{config::Config, general_setup};

mod commands;
mod events;
mod tasks;

/// vZDV Discord bot.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Load the config from a specific file.
    ///
    /// [default: vzdv.toml]
    #[arg(long)]
    config: Option<PathBuf>,

    /// Load the IDS config
    ///
    /// [default: ids.json]
    #[arg(long)]
    ids_config: Option<PathBuf>,

    /// Enable debug logging
    #[arg(short, long)]
    debug: bool,
}

/// Parse a bot ID from the token.
///
/// This function panics instead of returning a Result, as the token
/// must confirm to this layout in order to be valid for Discord.
fn bot_id_from_token(token: &str) -> u64 {
    std::str::from_utf8(
        &general_purpose::STANDARD_NO_PAD
            .decode(token.split('.').next().unwrap())
            .unwrap(),
    )
    .unwrap()
    .parse()
    .unwrap()
}

/// Entrypoint.
#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let (config, db, _) = general_setup(cli.debug, "vzdv_bot", cli.config, cli.ids_config).await;
    let config = Arc::new(config);

    let token = &config.discord.bot_token;
    let bot_id = bot_id_from_token(token);
    let intents = Intents::GUILD_MEMBERS;
    let mut shard = Shard::new(ShardId::ONE, token.clone(), intents);
    let http = Arc::new(HttpClient::new(token.clone()));
    let interaction_client = http.interaction(Id::new(bot_id));

    interaction_client
        .set_global_commands(&[
            commands::EventCommand::create_command().into(),
            commands::ResourcesCommand::create_command().into(),
        ])
        .await
        .expect("Could not register commands");

    debug!("Spawning background tasks");

    {
        let config = config.clone();
        let db = db.clone();
        let http = http.clone();
        tokio::spawn(async move {
            tasks::online::process(config, db, http).await;
        });
    };

    {
        let config = config.clone();
        let db = db.clone();
        let http = http.clone();
        tokio::spawn(async move {
            tasks::roles::process(config, db, http).await;
        });
    };

    {
        let config = config.clone();
        let db = db.clone();
        let http = http.clone();
        tokio::spawn(async move {
            tasks::off_roster::process(config, db, http).await;
        });
    };

    {
        let config = config.clone();
        let db = db.clone();
        let http = http.clone();
        tokio::spawn(async move {
            tasks::solo_certs::process(config, db, http).await;
        });
    };

    {
        let config = config.clone();
        let db = db.clone();
        let http = http.clone();
        tokio::spawn(async move {
            tasks::no_shows::process(config, db, http).await;
        });
    };

    {
        let config = config.clone();
        let db = db.clone();
        let http = http.clone();
        tokio::spawn(async move {
            tasks::streamers::process(config, db, http).await;
        });
    };

    info!("Connected to Gateway");
    loop {
        let event = match shard.next_event().await {
            Ok(event) => event,
            Err(source) => {
                warn!("Error receiving event: {source:?}");
                if source.is_fatal() {
                    break;
                }
                continue;
            }
        };
        let http = http.clone();
        let config = config.clone();
        let db: Pool<Sqlite> = db.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_event(event, http, bot_id, &config, &db).await {
                error!("Error in future: {e}");
            }
        });
    }
}

/// Handle all events send through the Gateway connection.
async fn handle_event(
    event: Event,
    http: Arc<HttpClient>,
    bot_id: u64,
    config: &Arc<Config>,
    db: &Pool<Sqlite>,
) -> Result<()> {
    commands::handler(&event, &http, bot_id, config, db).await?;
    events::handler(&event, &http, config, db).await?;

    Ok(())
}
