use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use log::{debug, info, warn};
use sqlx::{Pool, Sqlite};
use twilight_gateway::Event;
use twilight_http::{Client, client::InteractionClient};
use twilight_interactions::command::{CommandModel, CreateCommand};
use twilight_model::{
    application::interaction::InteractionData,
    channel::message::{
        Component, MessageFlags,
        component::{ActionRow, Button, ButtonStyle, SelectMenu, SelectMenuOption},
    },
    gateway::payload::incoming::InteractionCreate,
    http::interaction::InteractionResponse,
    id::Id,
};
use twilight_util::builder::{
    InteractionResponseDataBuilder,
    embed::{EmbedBuilder, EmbedFieldBuilder, ImageSource},
};
use vzdv::{
    config::Config,
    controller_can_see,
    sql::{self, Controller, EventPosition},
};

#[derive(Debug, CommandModel, CreateCommand)]
#[command(name = "event", desc = "Post event info or positions")]
pub struct EventCommand;

#[derive(Debug, CommandModel, CreateCommand)]
#[command(name = "resources", desc = "Link someone to the resources page")]
pub struct ResourcesCommand;

/// Build a simple ephemeral response with a `String` message.
fn quick_resp(message: &str) -> InteractionResponse {
    InteractionResponse {
        kind: twilight_model::http::interaction::InteractionResponseType::ChannelMessageWithSource,
        data: Some(
            InteractionResponseDataBuilder::new()
                .flags(MessageFlags::EPHEMERAL)
                .content(message)
                .build(),
        ),
    }
}

async fn setup<'a>(
    event: &'a Event,
    db: &Pool<Sqlite>,
    interaction: &InteractionClient<'_>,
) -> Result<Option<&'a Box<InteractionCreate>>> {
    if let Event::InteractionCreate(event) = event {
        // author ID check
        let user_id = match event.author_id() {
            Some(id) => id,
            None => {
                // I don't know when this would be triggered
                interaction
                    .create_response(
                        event.id,
                        &event.token,
                        &quick_resp("Discord isn't sharing your user ID"),
                    )
                    .await?;
                return Ok(None);
            }
        };
        // controller lookup
        let controller: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_DISCORD_ID)
            .bind(user_id.get().to_string())
            .fetch_optional(db)
            .await?;
        let controller = match controller {
            Some(c) => c,
            None => {
                // unknown user
                interaction
                    .create_response(
                        event.id,
                        &event.token,
                        &quick_resp("You have not linked your Discord to the website"),
                    )
                    .await?;
                return Ok(None);
            }
        };
        // permissions check
        if !controller_can_see(&Some(controller), vzdv::PermissionsGroup::EventsTeam) {
            // insufficient permissions
            interaction
                .create_response(
                    event.id,
                    &event.token,
                    &quick_resp("This command is for event staff"),
                )
                .await?;
            return Ok(None);
        }
        // good to continue
        return Ok(Some(event));
    }
    // some other type of event; don't care
    Ok(None)
}

/// Command handler.
pub async fn handler(
    raw_event: &Event,
    http: &Arc<Client>,
    bot_id: u64,
    config: &Arc<Config>,
    db: &Pool<Sqlite>,
) -> Result<()> {
    let interaction = http.interaction(Id::new(bot_id));
    if let Some(event) = setup(raw_event, db, &interaction).await? {
        let author_id = event.author_id().unwrap();
        match &event.0.data.as_ref().unwrap() {
            InteractionData::ApplicationCommand(app_command) => {
                if app_command.name == "event" {
                    info!("Got event command by {author_id}; building dropdown");
                    let events: Vec<_> = {
                        let all: Vec<vzdv::sql::Event> =
                            sqlx::query_as(sql::GET_ALL_EVENTS).fetch_all(db).await?;
                        all.iter()
                            .filter(|event| event.end >= Utc::now())
                            .cloned()
                            .collect()
                    };
                    if events.is_empty() {
                        interaction.create_response(event.id, &event.token, &InteractionResponse {
                        kind: twilight_model::http::interaction::InteractionResponseType::ChannelMessageWithSource,
                        data: Some(InteractionResponseDataBuilder::new()
                            .content("No upcoming events found")
                            .flags(MessageFlags::EPHEMERAL)
                            .components(None)
                            .build()
                        ),
                    })
                    .await?;
                        return Ok(());
                    }
                    let component = Component::ActionRow(ActionRow {
                        components: vec![Component::SelectMenu(SelectMenu {
                            custom_id: String::from("event_selection"),
                            disabled: false,
                            max_values: Some(1),
                            min_values: Some(1),
                            options: events
                                .iter()
                                .map(|event| SelectMenuOption {
                                    default: false,
                                    description: None,
                                    emoji: None,
                                    label: event.name.clone(),
                                    value: event.id.to_string(),
                                })
                                .collect(),
                            placeholder: Some(String::from("Select an event")),
                        })],
                    });
                    debug!("Rendering event selection dropdown");
                    interaction.create_response(event.id, &event.token, &InteractionResponse {
                        kind: twilight_model::http::interaction::InteractionResponseType::ChannelMessageWithSource,
                        data: Some(InteractionResponseDataBuilder::new()
                            .content("Select an event")
                            .flags(MessageFlags::EPHEMERAL)
                            .components([component])
                            .build()
                        )}).await?;
                } else if app_command.name == "resources" {
                    debug!("Resources command being used");
                    interaction.create_response(event.id, &event.token, &InteractionResponse{
                        kind: twilight_model::http::interaction::InteractionResponseType::ChannelMessageWithSource,
                        data: Some(InteractionResponseDataBuilder::new()
                            .content("You can find the answer to your question in the [facility resources](https://zdvartcc.org/facility/resources)!")
                            .build()
                        )}).await?;
                }
            }
            InteractionData::MessageComponent(component) => {
                if component.custom_id == "event_selection" {
                    let event_id = match component.values.first() {
                        Some(id) => id,
                        None => {
                            warn!("No event id in dropdown selection");
                            return Ok(());
                        }
                    };
                    info!("Got event dropdown selection: {event_id}");
                    let component = Component::ActionRow(ActionRow {
                        components: Vec::from([
                            Component::Button(Button {
                                style: ButtonStyle::Primary,
                                emoji: None,
                                label: Some(String::from("Post overview")),
                                custom_id: Some(format!("action_overview,{event_id}")),
                                url: None,
                                disabled: false,
                            }),
                            Component::Button(Button {
                                style: ButtonStyle::Primary,
                                emoji: None,
                                label: Some(String::from("Post positions")),
                                custom_id: Some(format!("action_positions,{event_id}")),
                                url: None,
                                disabled: false,
                            }),
                        ]),
                    });
                    debug!("Rendering action buttons");
                    interaction.create_response(event.id, &event.token, &InteractionResponse {
                        kind: twilight_model::http::interaction::InteractionResponseType::UpdateMessage,
                        data: Some(InteractionResponseDataBuilder::new()
                            .content("Select an option")
                            .flags(MessageFlags::EPHEMERAL)
                            .components([component])
                            .build()
                        ),
                    })
                    .await?;
                } else if component.custom_id.starts_with("action_") {
                    let (action, event_id) = {
                        let mut split = component.custom_id.split(',');
                        let action = match split.next() {
                            Some(a) => a,
                            None => {
                                warn!("Could not find action in button click");
                                return Ok(());
                            }
                        };
                        let event_id = match split.next() {
                            Some(id) => id,
                            None => {
                                warn!("Could not find event ID in button click");
                                return Ok(());
                            }
                        };
                        (action, event_id)
                    };
                    info!("Got action {action} for event {event_id} by {author_id}");

                    // let event = let events: Vec<vzdv::sql::Event>
                    let db_event: Option<vzdv::sql::Event> = sqlx::query_as(sql::GET_EVENT)
                        .bind(event_id)
                        .fetch_optional(db)
                        .await?;
                    let db_event = match db_event {
                        Some(e) => e,
                        None => {
                            warn!("Could not find event with id {event_id}");
                            return Ok(());
                        }
                    };

                    let embed = {
                        let mut embed = EmbedBuilder::new()
                            .title(db_event.name)
                            .url(format!("{}/events/{event_id}", config.hosted_domain));
                        if let Some(url) = db_event.image_url {
                            embed = embed.image(ImageSource::url(url)?);
                        }
                        if action == "action_overview" {
                            let formatted_description = if db_event
                                .description
                                .as_ref()
                                .is_some_and(|d| d.len() > 1024)
                            {
                                format!("{}...", &db_event.description.as_ref().unwrap()[..1021])
                            } else {
                                db_event.description.unwrap_or_default()
                            };

                            embed = embed
                                .field(
                                    EmbedFieldBuilder::new(
                                        "Start",
                                        format!(
                                            "<t:{}:f>",
                                            db_event.start.timestamp_millis() / 1_000
                                        ),
                                    )
                                    .inline(),
                                )
                                .field(
                                    EmbedFieldBuilder::new(
                                        "End",
                                        format!(
                                            "<t:{}:f>",
                                            db_event.end.timestamp_millis() / 1_000
                                        ),
                                    )
                                    .inline(),
                                )
                                .field(EmbedFieldBuilder::new(
                                    "Description",
                                    formatted_description,
                                ));
                        } else {
                            let controllers: Vec<Controller> =
                                sqlx::query_as(sql::GET_ALL_CONTROLLERS)
                                    .fetch_all(db)
                                    .await?;
                            let positions: Vec<EventPosition> =
                                sqlx::query_as(sql::GET_EVENT_POSITIONS)
                                    .bind(event_id)
                                    .fetch_all(db)
                                    .await?;
                            for position in positions {
                                let val = match position.cid {
                                    Some(cid) => {
                                        let controller = controllers.iter().find(|c| c.cid == cid);
                                        match controller {
                                            Some(c) => match &c.discord_id {
                                                Some(d_id) => format!("<@{d_id}>"),
                                                None => format!("{} {}", c.first_name, c.last_name),
                                            },
                                            None => String::from("Unknown"),
                                        }
                                    }
                                    None => String::from("Unassigned"),
                                };
                                embed = embed
                                    .field(EmbedFieldBuilder::new(&position.name, val).inline());
                            }
                            embed = embed.description("Position assignments");
                        }
                        embed.validate()?.build()
                    };
                    interaction.create_response(event.id, &event.token, &InteractionResponse {
                        kind: twilight_model::http::interaction::InteractionResponseType::UpdateMessage,
                        data: Some(InteractionResponseDataBuilder::new().content("Info posted").flags(MessageFlags::EPHEMERAL).components(None).build())
                    }).await?;
                    http.create_message(event.channel.as_ref().unwrap().id)
                        .embeds(&[embed])?
                        .await?;
                }
            }
            _ => {}
        }
    }

    Ok(())
}
