//! vZDV site/tasks/bot core and shared logic.

#![deny(clippy::all)]
#![deny(unsafe_code)]

use anyhow::{Result, anyhow, bail};
use config::Config;
use db::load_db;
use fern::{
    Dispatch,
    colors::{Color, ColoredLevelConfig},
};
use itertools::Itertools;
use log::{debug, error};
use reqwest::ClientBuilder;
use sql::Controller;
use sqlx::{Pool, Row, Sqlite, sqlite::SqliteRow};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::LazyLock,
    time::SystemTime,
};

pub mod activity;
pub mod aviation;
pub mod config;
pub mod db;
pub mod email;
pub mod kden;
pub mod sql;
pub mod vatsim;
pub mod vatusa;
pub mod vnas;

// I don't know what this is, but there's a SUP in ZDV that has this rating.
const IGNORE_MISSING_STAFF_POSITIONS_FOR: [&str; 1] = ["FACCBT"];

/// HTTP client for making external requests.
///
/// Include an HTTP user agent of the project's repo for contact.
pub static GENERAL_HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    ClientBuilder::new()
        .user_agent("github.com/zdv-artcc/vzdv")
        .build()
        .expect("Could not construct HTTP client")
});

/// Check whether the VATSIM session position is in this facility's airspace.
///
/// Relies on the config's "stats.position_prefixes" and suffixes.
pub fn position_in_facility_airspace(config: &Config, position: &str) -> bool {
    let prefix_match = config
        .stats
        .position_prefixes
        .iter()
        .any(|prefix| position.starts_with(prefix));
    if !prefix_match {
        return false;
    }
    config
        .stats
        .position_suffixes
        .iter()
        .any(|suffix| position.ends_with(suffix))
}

/// Retrieve a mapping of controller CID to first and last names.
pub async fn get_controller_cids_and_names(
    db: &Pool<Sqlite>,
) -> Result<HashMap<u32, (String, String)>> {
    let mut cid_name_map = HashMap::new();
    let rows: Vec<SqliteRow> = sqlx::query(sql::GET_CONTROLLER_CIDS_AND_NAMES)
        .fetch_all(db)
        .await?;
    rows.iter().for_each(|row| {
        let cid: u32 = row.try_get("cid").unwrap();
        let first_name: String = row.try_get("first_name").unwrap();
        let last_name: String = row.try_get("last_name").unwrap();
        cid_name_map.insert(cid, (first_name, last_name));
    });
    Ok(cid_name_map)
}

/// Determine the staff position of the controller.
///
/// VATUSA does not differentiate between the official staff position (say, FE)
/// and their assistants (e.g. AFE). At the VATUSA level, they're the same. Here,
/// we do want to determine that difference.
///
/// This function will return all positions in the event the controller holds more
/// than one, like being an Instructor and also the FE, or a Mentor and an AEC.
pub fn determine_staff_positions(controller: &Controller) -> Vec<String> {
    let roles: HashSet<_> = controller
        .roles
        .split_terminator(',')
        .filter(|r| !IGNORE_MISSING_STAFF_POSITIONS_FOR.contains(r))
        .collect();
    roles.iter().map(|&r| r.to_owned()).collect()
}

pub enum ControllerRating {
    INA,
    SUS,
    OBS,
    S1,
    S2,
    S3,
    C1,
    C2,
    C3,
    I1,
    I2,
    I3,
    SUP,
    ADM,
}

impl ControllerRating {
    /// Enum values as a string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::INA => "INA",
            Self::SUS => "SUS",
            Self::OBS => "OBS",
            Self::S1 => "S1",
            Self::S2 => "S2",
            Self::S3 => "S3",
            Self::C1 => "C1",
            Self::C2 => "C2",
            Self::C3 => "C3",
            Self::I1 => "I1",
            Self::I2 => "I2",
            Self::I3 => "I3",
            Self::SUP => "SUP",
            Self::ADM => "ADM",
        }
    }

    pub fn as_id(&self) -> i8 {
        match self {
            Self::INA => -1,
            Self::SUS => 0,
            Self::OBS => 1,
            Self::S1 => 2,
            Self::S2 => 3,
            Self::S3 => 4,
            Self::C1 => 5,
            Self::C2 => 6,
            Self::C3 => 7,
            Self::I1 => 8,
            Self::I2 => 9,
            Self::I3 => 10,
            Self::SUP => 11,
            Self::ADM => 12,
        }
    }
}

impl TryFrom<i8> for ControllerRating {
    type Error = anyhow::Error;

    fn try_from(value: i8) -> std::result::Result<Self, Self::Error> {
        match value {
            -1 => Ok(Self::INA),
            0 => Ok(Self::SUS),
            1 => Ok(Self::OBS),
            2 => Ok(Self::S1),
            3 => Ok(Self::S2),
            4 => Ok(Self::S3),
            5 => Ok(Self::C1),
            6 => Ok(Self::C2),
            7 => Ok(Self::C3),
            8 => Ok(Self::I1),
            9 => Ok(Self::I2),
            10 => Ok(Self::I3),
            11 => Ok(Self::SUP),
            12 => Ok(Self::ADM),
            _ => Err(anyhow!("Unknown controller rating")),
        }
    }
}

pub enum ControllerStatus {
    Active,
    Inactive,
    LeaveOfAbsence,
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, PartialEq)]
pub enum StaffPosition {
    None,
    ATM,
    DATM,
    TA,
    FE,
    EC,
    WM,
    ATA,
    AFE,
    AEC,
    AWM,
    INS,
    MTR,
}

impl StaffPosition {
    /// Enum value as a string.
    ///
    /// "None" roles is an empty string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "",
            Self::ATM => "ATM",
            Self::DATM => "DATM",
            Self::TA => "TA",
            Self::FE => "FE",
            Self::EC => "EC",
            Self::WM => "WM",
            Self::ATA => "ATA",
            Self::AFE => "AFE",
            Self::AEC => "AEC",
            Self::AWM => "AWM",
            Self::INS => "INS",
            Self::MTR => "MTR",
        }
    }
}

impl From<&str> for StaffPosition {
    /// Inverse of `StaffPosition::as_str`.
    fn from(value: &str) -> Self {
        match value {
            "ATM" => StaffPosition::ATM,
            "DATM" => StaffPosition::DATM,
            "TA" => StaffPosition::TA,
            "FE" => StaffPosition::FE,
            "EC" => StaffPosition::EC,
            "WM" => StaffPosition::WM,
            "ATA" => StaffPosition::ATA,
            "AFE" => StaffPosition::AFE,
            "AEC" => StaffPosition::AEC,
            "AWM" => StaffPosition::AWM,
            "INS" => StaffPosition::INS,
            "MTR" => StaffPosition::MTR,
            _ => StaffPosition::None,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum PermissionsGroup {
    /// Literally anyone.
    Anon,
    /// Has a session.
    LoggedIn,
    /// Has a role.
    SomeStaff,
    /// Has a Jr or Sr position.
    NamedPosition,
    /// EC, AEC, and up.
    EventsTeam,
    /// MTR, INS, TA, and up.
    TrainingTeam,
    /// ATM, DATM (and WM).
    Admin,
}

/// Permissions control for accessing things.
pub fn controller_can_see(controller: &Option<Controller>, team: PermissionsGroup) -> bool {
    let controller = match controller {
        Some(c) => c,
        None => return team == PermissionsGroup::Anon,
    };
    let roles: Vec<_> = controller
        .roles
        .split(',')
        .map(StaffPosition::from)
        .collect();
    match team {
        PermissionsGroup::Anon => true,
        PermissionsGroup::LoggedIn => true,
        PermissionsGroup::NamedPosition => [
            StaffPosition::ATM,
            StaffPosition::DATM,
            StaffPosition::TA,
            StaffPosition::FE,
            StaffPosition::EC,
            StaffPosition::WM,
        ]
        .iter()
        .any(|r| roles.contains(r)),
        PermissionsGroup::SomeStaff => [
            StaffPosition::ATM,
            StaffPosition::DATM,
            StaffPosition::TA,
            StaffPosition::FE,
            StaffPosition::EC,
            StaffPosition::WM,
            StaffPosition::ATA,
            StaffPosition::AFE,
            StaffPosition::AEC,
            StaffPosition::AWM,
            StaffPosition::INS,
            StaffPosition::MTR,
        ]
        .iter()
        .any(|r| roles.contains(r)),
        PermissionsGroup::EventsTeam => [
            StaffPosition::EC,
            StaffPosition::AEC,
            StaffPosition::ATM,
            StaffPosition::DATM,
            StaffPosition::WM,
        ]
        .iter()
        .any(|r| roles.contains(r)),
        PermissionsGroup::TrainingTeam => [
            StaffPosition::MTR,
            StaffPosition::INS,
            StaffPosition::TA,
            StaffPosition::ATA,
            StaffPosition::ATM,
            StaffPosition::DATM,
            StaffPosition::WM,
        ]
        .iter()
        .any(|r| roles.contains(r)),
        PermissionsGroup::Admin => [
            StaffPosition::ATM,
            StaffPosition::DATM,
            StaffPosition::TA,
            StaffPosition::WM,
        ]
        .iter()
        .any(|r| roles.contains(r)),
    }
}

/// Setup logging, load the config, connect to the DB; return config and DB.
///
/// Exit the process with an error code if anything goes wrong.
pub async fn general_setup(
    debug_logging: bool,
    binary_name: &str,
    config_path: Option<PathBuf>,
) -> (Config, Pool<Sqlite>) {
    let colors_line = ColoredLevelConfig::new()
        .error(Color::Red)
        .warn(Color::Yellow)
        .info(Color::Green)
        .debug(Color::Blue);
    Dispatch::new()
        .level(log::LevelFilter::Info)
        .level_for("tracing", log::LevelFilter::Warn)
        .level_for("twilight_gateway_queue", log::LevelFilter::Warn)
        .level_for("twilight_gateway::shard", log::LevelFilter::Warn)
        .level_for(
            "twilight_http_ratelimiting::in_memory::bucket",
            log::LevelFilter::Warn,
        )
        .level_for(
            "vzdv",
            if debug_logging {
                log::LevelFilter::Debug
            } else {
                log::LevelFilter::Info
            },
        )
        .level_for(
            "vzdv_site",
            if debug_logging {
                log::LevelFilter::Debug
            } else {
                log::LevelFilter::Info
            },
        )
        .level_for(
            "vzdv_bot",
            if debug_logging {
                log::LevelFilter::Debug
            } else {
                log::LevelFilter::Info
            },
        )
        .level_for(
            "vzdv_tasks",
            if debug_logging {
                log::LevelFilter::Debug
            } else {
                log::LevelFilter::Info
            },
        )
        .level_for(
            "vzdv_import",
            if debug_logging {
                log::LevelFilter::Debug
            } else {
                log::LevelFilter::Info
            },
        )
        .chain(
            Dispatch::new()
                .format(move |out, message, record| {
                    out.finish(format_args!(
                        "[{} {} {}] {}",
                        humantime::format_rfc3339_seconds(SystemTime::now()),
                        colors_line.color(record.level()),
                        record.target(),
                        message,
                    ))
                })
                .chain(std::io::stdout()),
        )
        .chain(
            Dispatch::new()
                .format(move |out, message, record| {
                    out.finish(format_args!(
                        "[{} {} {}] {}",
                        humantime::format_rfc3339_seconds(SystemTime::now()),
                        record.level(),
                        record.target(),
                        message,
                    ))
                })
                .chain(
                    fern::log_file(format!("{binary_name}.log")).expect("Could not open log file"),
                ),
        )
        .apply()
        .expect("Error configuring logging");
    debug!("Logging configured");

    let config_location = match config_path {
        Some(path) => path,
        None => Path::new(config::DEFAULT_CONFIG_FILE_NAME).to_owned(),
    };
    debug!("Loading from config file");
    let config = match Config::load_from_disk(&config_location) {
        Ok(c) => c,
        Err(e) => {
            error!("Could not load config: {e}");
            std::process::exit(1);
        }
    };
    debug!("Creating DB connection");
    let db = match load_db(&config).await {
        Ok(db) => db,
        Err(e) => {
            error!("Could not load DB: {e}");
            std::process::exit(1);
        }
    };

    (config, db)
}

/// Retrieve all OIs that are currently in use.
pub async fn retrieve_all_in_use_ois(db: &Pool<Sqlite>) -> Result<Vec<String>> {
    let in_use: Vec<String> = sqlx::query(sql::GET_ALL_OIS)
        .fetch_all(db)
        .await?
        .iter()
        .map(|row| {
            row.try_get("operating_initials")
                .expect("Could not get 'operating_initials' column from DB")
        })
        .collect();
    Ok(in_use)
}

/// Generate new unique OIs for the controller.
pub fn generate_operating_initials_for(
    in_use: &[String],
    first_name: &str,
    last_name: &str,
) -> Result<String> {
    let first_first = first_name
        .chars()
        .next()
        .ok_or(anyhow!("Empty first name?"))?
        .to_uppercase()
        .next()
        .ok_or(anyhow!("Weird first name first char?"))?;
    let last_first = last_name
        .chars()
        .next()
        .ok_or(anyhow!("Empty last name?"))?
        .to_uppercase()
        .next()
        .ok_or(anyhow!("Weird last name first char?"))?;

    // first try their actual initials
    let direct_from_name = format!("{first_first}{last_first}");
    if !in_use.contains(&direct_from_name) {
        return Ok(direct_from_name);
    }

    // attempt first initial with the next available second char
    let last_first_digit = {
        let digit: u32 = last_first.into();
        let digit = digit - 65;
        digit as u8
    };
    for i in last_first_digit..25 {
        let c: char = (i + 65).into();
        let attempt = format!("{first_first}{c}");
        if !in_use.contains(&attempt) {
            return Ok(attempt);
        }
    }

    // attempt first global alphabetical pairing
    for a in 0..25 {
        for b in 0..25 {
            let c: char = (a + 65).into();
            let d: char = (b + 65).into();
            let attempt = format!("{c}{d}");
            if !in_use.contains(&attempt) {
                return Ok(attempt);
            }
        }
    }

    // should never hit this
    bail!("Apparently there are no OIs available")
}

/// Get controllers from the DB with that role.
pub async fn get_staff_member_by_role(db: &Pool<Sqlite>, role: &str) -> Result<Vec<Controller>> {
    let with_roles: Vec<Controller> = sqlx::query_as(sql::GET_CONTROLLERS_WITH_ROLES)
        .fetch_all(db)
        .await?;
    Ok(with_roles
        .iter()
        .filter(|c| c.roles.split_terminator(',').contains(&role))
        .cloned()
        .collect())
}

#[allow(clippy::field_reassign_with_default)]
#[cfg(test)]
pub mod tests {
    use super::{
        PermissionsGroup, controller_can_see, determine_staff_positions,
        position_in_facility_airspace,
    };
    use crate::{
        config::Config, generate_operating_initials_for, sql::Controller,
        vatsim::parse_vatsim_timestamp,
    };

    #[test]
    fn test_parse_vatsim_timestamp() {
        parse_vatsim_timestamp("2024-03-02T16:20:37.0439318Z").unwrap();
    }

    #[test]
    fn test_position_in_facility_airspace() {
        let mut config = Config::default();
        config.stats.position_prefixes.push("DEN_".to_string());
        config.stats.position_suffixes.push("_TWR".to_string());

        assert!(position_in_facility_airspace(&config, "DEN_2_TWR"));
        assert!(!position_in_facility_airspace(&config, "SAN_GND"));
        assert!(!position_in_facility_airspace(&config, "DENN_F_TWR"));
        assert!(!position_in_facility_airspace(&config, "1111111_OBS"));
    }

    #[test]
    fn test_determine_staff_positions_empty() {
        let mut controller = Controller::default();
        controller.cid = 123;

        assert!(determine_staff_positions(&controller).is_empty());
    }

    #[test]
    fn test_determine_staff_positions_shared() {
        let mut controller = Controller::default();
        controller.cid = 123;
        controller.roles = "MTR".to_owned();

        assert_eq!(determine_staff_positions(&controller), vec!["MTR"]);
    }

    #[test]
    fn test_determine_staff_positions_single() {
        let mut controller = Controller::default();
        controller.cid = 123;
        controller.roles = "FE".to_owned();

        assert_eq!(determine_staff_positions(&controller), vec!["FE"]);
    }

    #[test]
    fn test_determine_staff_positions_single_assistant() {
        let mut controller = Controller::default();
        controller.cid = 123;
        controller.roles = "AFE".to_owned();

        assert_eq!(determine_staff_positions(&controller), vec!["AFE"]);
    }

    #[test]
    fn test_determine_staff_positions_ignore() {
        let mut controller = Controller::default();
        controller.cid = 123;
        controller.roles = "FACCBT".to_owned();

        assert!(determine_staff_positions(&controller).is_empty());
    }

    #[test]
    fn test_controller_can_see_anon() {
        assert!(controller_can_see(&None, PermissionsGroup::Anon));
        let mut controller = Controller::default();
        assert!(controller_can_see(
            &Some(controller.clone()),
            PermissionsGroup::Anon
        ));
        controller.roles = "DATM,INS".to_string();
        assert!(controller_can_see(
            &Some(controller),
            PermissionsGroup::Anon
        ));
    }

    #[test]
    fn test_controller_can_see_logged_in() {
        assert!(!controller_can_see(&None, PermissionsGroup::LoggedIn));
        let mut controller = Controller::default();
        assert!(controller_can_see(
            &Some(controller.clone()),
            PermissionsGroup::LoggedIn
        ));
        controller.roles = "DATM,INS".to_string();
        assert!(controller_can_see(
            &Some(controller.clone()),
            PermissionsGroup::LoggedIn
        ));
    }

    #[test]
    fn test_controller_can_see_teams() {
        assert!(!controller_can_see(&None, PermissionsGroup::EventsTeam));
        let mut controller = Controller::default();
        assert!(!controller_can_see(
            &Some(controller.clone()),
            PermissionsGroup::EventsTeam
        ));
        controller.roles = "EC".to_string();
        assert!(controller_can_see(
            &Some(controller.clone()),
            PermissionsGroup::EventsTeam
        ));
        controller.roles = "AEC".to_string();
        assert!(controller_can_see(
            &Some(controller.clone()),
            PermissionsGroup::EventsTeam
        ));

        controller.roles = "MTR".to_string();
        assert!(!controller_can_see(
            &Some(controller.clone()),
            PermissionsGroup::EventsTeam
        ));
        assert!(controller_can_see(
            &Some(controller.clone()),
            PermissionsGroup::TrainingTeam
        ));
    }

    #[test]
    fn test_controller_can_see_admin() {
        assert!(!controller_can_see(&None, PermissionsGroup::Admin));
        let mut controller = Controller::default();
        assert!(!controller_can_see(
            &Some(controller.clone()),
            PermissionsGroup::Admin
        ));
        controller.roles = "EC".to_string();
        assert!(!controller_can_see(
            &Some(controller.clone()),
            PermissionsGroup::Admin
        ));
        controller.roles = "ATM".to_string();
        assert!(controller_can_see(
            &Some(controller.clone()),
            PermissionsGroup::Admin
        ));
        controller.roles = "DATM".to_string();
        assert!(controller_can_see(
            &Some(controller.clone()),
            PermissionsGroup::Admin
        ));
        controller.roles = "WM".to_string();
        assert!(controller_can_see(
            &Some(controller.clone()),
            PermissionsGroup::Admin
        ));
    }

    #[test]
    fn test_generate_operating_initials_for() {
        let in_use = &[
            String::from("AA"),
            String::from("AE"),
            String::from("BC"),
            String::from("RY"),
            String::from("RZ"),
        ];

        // normal
        let result = generate_operating_initials_for(in_use, "John", "Smith").unwrap();
        assert_eq!(&result, "JS");

        // next is available
        let result = generate_operating_initials_for(in_use, "aaron", "Edwards").unwrap();
        assert_eq!(&result, "AF");

        // wrap around
        let result = generate_operating_initials_for(in_use, "Ron", "Yo").unwrap();
        assert_eq!(&result, "AB");
    }
}
