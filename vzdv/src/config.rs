use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::Path};

use crate::ids::AirportProcedure;

/// Default place to look for the config file.
pub const DEFAULT_CONFIG_FILE_NAME: &str = "vzdv.toml";
pub const DEFAULT_IDS_CONFIG_FILE_NAME: &str = "ids.json";

/// App configuration.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    pub hosted_domain: String,
    pub database: ConfigDatabase,
    pub staff: ConfigStaff,
    pub vatsim: ConfigVatsim,
    pub training: ConfigTraining,
    pub airports: ConfigAirports,
    pub weather: ConfigWeather,
    pub stats: ConfigStats,
    pub discord: ConfigDiscord,
    pub email: ConfigEmail,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigDatabase {
    pub file: String,
    pub resource_category_ordering: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigStaff {
    pub email_domain: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigVatsim {
    pub oauth_url_base: String,
    pub oauth_client_id: String,
    pub oauth_client_secret: String,
    pub oauth_client_callback_url: String,
    pub vatusa_api_key: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigTraining {
    pub certifications: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigAirports {
    pub all: Vec<Airport>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigWeather {
    pub overview: Vec<String>,
    pub all: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Airport {
    pub code: String,
    pub name: String,
    pub location: String,
    pub towered: bool,
    pub class: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigStats {
    pub position_prefixes: Vec<String>,
    pub position_suffixes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigDiscord {
    pub join_link: String,
    pub bot_token: String,
    pub auth: ConfigDiscordAuth,
    pub guild_id: u64,
    pub online_channel: u64,
    pub online_message: Option<u64>,
    pub off_roster_channel: u64,
    pub webhooks: ConfigDiscordWebhooks,
    pub roles: ConfigDiscordRoles,
    pub owner_id: u64,
    pub solo_cert_expiration_channel: u64,
    pub streamers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigDiscordAuth {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigDiscordWebhooks {
    pub staffing_request: String,
    pub new_feedback: String,
    pub feedback: String,
    pub new_visitor_app: String,
    pub errors: String,
    pub audit: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigDiscordRoles {
    // status
    pub guest: u64,
    pub controller_otm: u64,
    pub home_controller: u64,
    pub visiting_controller: u64,
    pub event_controller: u64,

    // special
    pub vatusa_vatgov: u64,

    // staff
    pub sr_staff: u64,
    pub jr_staff: u64,

    // staff teams
    pub training_staff: u64,
    pub event_team: u64,
    pub fe_team: u64,
    pub web_team: u64,

    // ratings
    pub administrator: u64,
    pub supervisor: u64,
    pub instructor_3: u64,
    pub instructor_1: u64,
    pub controller_3: u64,
    pub controller_1: u64,
    pub student_3: u64,
    pub student_2: u64,
    pub student_1: u64,
    pub observer: u64,

    // certs
    pub t2_ctr: u64,
    pub t1_app: u64,
    pub t1_twr: u64,
    pub t1_gnd: u64,

    // misc
    pub ignore: u64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigEmail {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub from: String,
    pub reply_to: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigIDS(pub HashMap<String, AirportProcedure>);

impl ConfigIDS {
    /// Read the JSON file at the given path and load into the app's configuration file.
    pub fn load_from_disk(path: &Path) -> Result<Self> {
        if !Path::new(path).exists() {
            bail!("Config file \"{}\" not found", path.display());
        }
        let text = fs::read_to_string(path)?;
        let config: ConfigIDS = serde_json::from_str(&text)?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        for (icao, entry) in self.0.iter() {
            match entry {
                AirportProcedure::Combined(proc) => {
                    // Check that all flow keys match their name field
                    if let Some((bad_flow_name, bad_flow)) =
                        proc.flows.iter().find(|f| *f.0 != f.1.name)
                    {
                        bail!(
                            "{bad_flow_name} flow name ({}) does not match for {icao}",
                            bad_flow.name
                        )
                    }
                    // Check that every rule mentions a flow present in `flows`
                    if let Some(bad_rule) = proc
                        .rules
                        .iter()
                        .find(|r| !proc.flows.contains_key(&r.use_flow))
                    {
                        bail!(
                            "Rule in {icao} references a flow that is not present in flows field! Rule: {bad_rule:?}"
                        )
                    }
                }
                AirportProcedure::Split(proc) => {
                    // Check that every rule references valid dep and arr flows
                    for rule in &proc.rules {
                        if !proc.dep_flows.contains_key(&rule.use_dep_flow) {
                            bail!(
                                "Rule in {icao} references dep flow '{}' not present in depFlows! Rule: {rule:?}",
                                rule.use_dep_flow
                            )
                        }
                        if !proc.arr_flows.contains_key(&rule.use_arr_flow) {
                            bail!(
                                "Rule in {icao} references arr flow '{}' not present in arrFlows! Rule: {rule:?}",
                                rule.use_arr_flow
                            )
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

impl Config {
    /// Read the TOML file at the given path and load into the app's configuration file.
    pub fn load_from_disk(path: &Path) -> Result<Self> {
        if !Path::new(path).exists() {
            bail!("Config file \"{}\" not found", path.display());
        }
        let text = fs::read_to_string(path)?;
        let config: Config = toml::from_str(&text)?;
        Ok(config)
    }
}
