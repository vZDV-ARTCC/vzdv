use anyhow::{Result, anyhow};
use log::warn;
use serde::{Deserialize, Serialize};

/// Derived weather conditions, from best to worst.
#[allow(clippy::upper_case_acronyms)]
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
pub enum WeatherConditions {
    VFR,
    MVFR,
    IFR,
    LIFR,
}

impl WeatherConditions {
    /// Flight rules for a ceiling in feet.
    pub fn for_ceiling(feet: u16) -> Self {
        match feet {
            0..500 => Self::LIFR,
            500..1_000 => Self::IFR,
            1_000..=3_000 => Self::MVFR,
            _ => Self::VFR,
        }
    }

    /// Flight rules for a visibility in whole statute miles.
    pub fn for_visibility(miles: u16) -> Self {
        match miles {
            0 => Self::LIFR,
            1..3 => Self::IFR,
            3..=5 => Self::MVFR,
            _ => Self::VFR,
        }
    }
}

/// A run of METAR text, with flight rules if it's a ceiling or visibility below VFR.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetarPart {
    pub text: String,
    pub conditions: Option<WeatherConditions>,
}

/// Parsed weather information for an airport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AirportWeather {
    pub name: String,
    pub conditions: WeatherConditions,
    pub visibility: u16,
    pub ceiling: u16,
    pub wind: (u16, u8, u8),
    pub altimeter: Option<f32>,
    pub raw: String,
}

/// Parse a METAR into a struct of data.
pub fn parse_metar(line: &str) -> Result<AirportWeather> {
    let parts: Vec<_> = line.split(' ').collect();
    let airport = {
        let s = parts.first().ok_or_else(|| anyhow!("Blank metar?"))?;
        if s.len() == 4 { &s[1..] } else { s }
    };
    // remarks can repeat cloud and visibility groups in other formats
    let body: Vec<_> = parts
        .iter()
        .copied()
        .take_while(|part| *part != "RMK")
        .collect();
    let ceiling = body
        .iter()
        .find_map(|part| ceiling_layer(part))
        .unwrap_or(3_456);
    let visibility = body
        .iter()
        .enumerate()
        .find_map(|(i, part)| visibility_miles(part, i.checked_sub(1).map(|prev| body[prev])))
        .unwrap_or_else(|| {
            warn!("Could not determine visibility for {airport}");
            0
        });

    let wind_info = parts
        .iter()
        .find(|part| part.ends_with("KT"))
        .unwrap_or_else(|| {
            warn!("Could not determine wind for {airport}");
            &"00000KT"
        });
    let wind = {
        let dir = {
            let s = wind_info.chars().take(3).collect::<String>();
            if s == "VRB" { 0 } else { s.parse()? }
        };
        let mag = wind_info
            .chars()
            .skip(3)
            .take(2)
            .collect::<String>()
            .parse()?;
        let gust = wind_info
            .chars()
            .skip(6)
            .take_while(|c| c.is_numeric())
            .collect::<String>();
        let gust = if !gust.is_empty() { gust.parse()? } else { 0 };
        (dir, mag, gust)
    };

    let conditions =
        WeatherConditions::for_ceiling(ceiling).max(WeatherConditions::for_visibility(visibility));

    let altimeter = parts.iter().find_map(|part| {
        if let Some(v) = part.strip_prefix('A') {
            // the `.parse().ok()` should handle "AUTO" as None and let it continue
            v.parse::<u32>()
                .ok()
                .map(|hundredths| hundredths as f32 / 100.0)
        } else {
            None
        }
    });

    Ok(AirportWeather {
        name: airport.to_string(),
        conditions,
        visibility,
        ceiling,
        wind,
        altimeter,
        raw: line.to_string(),
    })
}

/// Height in feet of a broken, overcast or vertical visibility layer, e.g. "OVC008" or "VV002".
fn ceiling_layer(group: &str) -> Option<u16> {
    let height = ["BKN", "OVC", "VV"]
        .iter()
        .find_map(|prefix| group.strip_prefix(prefix))?;
    let hundreds = height.get(..3)?;
    if !hundreds.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(hundreds.parse::<u16>().ok()?.saturating_mul(100))
}

/// Whole statute miles from a visibility group like "10SM", "1/2SM" or "M1/4SM".
/// `previous` is the group before it, for the "1" in "1 1/2SM".
fn visibility_miles(group: &str, previous: Option<&str>) -> Option<u16> {
    let miles = group.strip_suffix("SM")?;
    let miles = miles
        .strip_prefix('P')
        .or_else(|| miles.strip_prefix('M'))
        .unwrap_or(miles);
    let Some((numerator, denominator)) = miles.split_once('/') else {
        return miles.parse().ok();
    };
    numerator.parse::<u16>().ok()?;
    denominator.parse::<u16>().ok()?;
    // a fraction is less than a mile, plus any whole miles before it
    Some(previous.and_then(|whole| whole.parse().ok()).unwrap_or(0))
}

/// Split a METAR into runs of text, marking the ceilings and visibilities below VFR.
pub fn highlight_metar(metar: &str) -> Vec<MetarPart> {
    let groups: Vec<_> = metar.split_whitespace().collect();
    let body = groups
        .iter()
        .position(|group| *group == "RMK")
        .unwrap_or(groups.len());
    let mut parts: Vec<MetarPart> = Vec::new();
    let mut push = |text: String, conditions: Option<WeatherConditions>| {
        if let Some(last) = parts.last_mut() {
            if last.conditions.is_none() {
                last.text.push(' ');
            } else {
                parts.push(MetarPart {
                    text: " ".to_string(),
                    conditions: None,
                });
            }
        }
        match parts.last_mut() {
            Some(last) if conditions.is_none() && last.conditions.is_none() => {
                last.text.push_str(&text)
            }
            _ => parts.push(MetarPart { text, conditions }),
        }
    };

    let mut i = 0;
    while i < groups.len() {
        let group = groups[i];
        let next = groups.get(i + 1).filter(|_| i + 1 < body);
        let (len, conditions) = if i >= body {
            (1, None)
        } else if let Some(miles) = next
            .filter(|_| group.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|next| visibility_miles(next, Some(group)))
        {
            (2, Some(WeatherConditions::for_visibility(miles)))
        } else if let Some(miles) = visibility_miles(group, None) {
            (1, Some(WeatherConditions::for_visibility(miles)))
        } else {
            (1, ceiling_layer(group).map(WeatherConditions::for_ceiling))
        };
        let conditions = conditions.filter(|c| *c != WeatherConditions::VFR);
        push(groups[i..i + len].join(" "), conditions);
        i += len;
    }
    parts
}

/// Check if the value is AOA `min`, clockwise to AOB `max`.
pub fn wind_between(val: u16, min: u16, max: u16) -> bool {
    let val = val % 360;
    let min = min % 360;
    let max = max % 360;

    if min <= max {
        min <= val && val <= max
    } else {
        min <= val || val <= max
    }
}

#[cfg(test)]
pub mod tests {
    use super::{MetarPart, WeatherConditions, highlight_metar, parse_metar, wind_between};

    #[test]
    fn test_parse_metar_standard() {
        let ret = parse_metar("KDEN 030253Z 22013KT 10SM SCT100 BKN160 13/M12 A2943 RMK AO2 PK WND 21036/0211 SLP924 T01331117 58005").unwrap();
        assert_eq!(ret.name, "DEN");
        assert_eq!(ret.conditions, WeatherConditions::VFR);

        let ret = parse_metar("KDEN 2SM BNK005").unwrap();
        assert_eq!(ret.conditions, WeatherConditions::IFR);

        let ret = parse_metar("KDEN 4SM OVC020").unwrap();
        assert_eq!(ret.conditions, WeatherConditions::MVFR);

        let ret = parse_metar("KDEN 1/2SM OVC001").unwrap();
        assert_eq!(ret.conditions, WeatherConditions::LIFR);
    }

    #[test]
    fn test_parse_metar_wind() {
        let missing = "K5SM 070435Z AUTO 10SM CLR M12/M13 A3015 RMK AO2";
        let ret = parse_metar(missing).unwrap();
        assert_eq!(ret.wind, (0, 0, 0));

        let standard = "KCPW 070435Z AUTO 03007KT OVC003 M13/M15 A3013 RMK AO2 PWINO";
        let ret = parse_metar(standard).unwrap();
        assert_eq!(ret.wind, (30, 7, 0));

        let gust = "KFLY 070435Z AUTO 36014G21KT OVC036 M05/M07 A3028 RMK AO2 T10531075 PWINO";
        let ret = parse_metar(gust).unwrap();
        assert_eq!(ret.wind, (360, 14, 21));
    }

    #[test]
    fn test_parse_metar_odd_ones() {
        let entries = &[
            "K4BM 070435Z AUTO 36006KT BKN009 OVC014 A3021 RMK AO2 PWINO",
            "K5SM 070435Z AUTO 10SM CLR M12/M13 A3015 RMK AO2",
            "KAEJ 242115Z AUTO 18/M10 A3011 RMK AO2 T01801100 PWINO",
            "KCPW 070435Z AUTO 03007KT OVC003 M13/M15 A3013 RMK AO2 PWINO",
            "KDWX 070435Z AUTO 36009KT CLR M07/M10 A3026 RMK AO2 T10681100 PWINO",
            "KFLY 070435Z AUTO 36014G21KT OVC036 M05/M07 A3028 RMK AO2 T10531075 PWINO",
            "KHEQ 070435Z AUTO 00/00 RMK AO2 TSNO PWINO",
            "KMYP 070435Z AUTO OVC002 M14/M16 A3018 RMK AO2 PWINO",
            "KTBX 070415Z AUTO 15017KT CLR M06/M12 A3027 RMK AO2 PWINO",
            "KAPA 081853Z VRB06KT 10SM SCT055 16/02 A3027 RMK AO2 SLP224 T01610017",
        ];

        for entry in entries {
            _ = parse_metar(entry).unwrap();
        }
    }

    #[test]
    fn test_parse_metar_ceilings_and_visibilities() {
        let conditions = |metar: &str| parse_metar(metar).unwrap().conditions;
        // category boundaries: a 1,000 ft ceiling is MVFR and 500 ft is IFR
        assert_eq!(conditions("KDEN 10SM OVC010"), WeatherConditions::MVFR);
        assert_eq!(conditions("KDEN 10SM OVC009"), WeatherConditions::IFR);
        assert_eq!(conditions("KDEN 10SM BKN005"), WeatherConditions::IFR);
        assert_eq!(conditions("KDEN 10SM BKN004"), WeatherConditions::LIFR);
        assert_eq!(conditions("KDEN 10SM OVC030"), WeatherConditions::MVFR);
        assert_eq!(conditions("KDEN 10SM OVC031"), WeatherConditions::VFR);
        assert_eq!(conditions("KDEN 5SM CLR"), WeatherConditions::MVFR);
        assert_eq!(conditions("KDEN 3SM CLR"), WeatherConditions::MVFR);
        // the worse of the two wins
        assert_eq!(conditions("KDEN 2SM OVC050"), WeatherConditions::IFR);
        assert_eq!(conditions("KDEN 10SM VV002"), WeatherConditions::LIFR);

        let ret = parse_metar("KDEN 041853Z 1 1/2SM BR OVC008CB 24/07 A3034").unwrap();
        assert_eq!(ret.visibility, 1);
        assert_eq!(ret.ceiling, 800);
        assert_eq!(ret.conditions, WeatherConditions::IFR);
        assert_eq!(parse_metar("KDEN M1/4SM FG").unwrap().visibility, 0);
        assert_eq!(parse_metar("KDEN P6SM SKC").unwrap().visibility, 6);

        // layers without heights are skipped, and remarks are ignored
        let ret =
            parse_metar("KDEN 041853Z AUTO 10SM BKN/// OVC045 A3034 RMK CIG 005V010 VIS 1/2SM")
                .unwrap();
        assert_eq!(ret.ceiling, 4_500);
        assert_eq!(ret.conditions, WeatherConditions::VFR);
        assert!(parse_metar("KDEN 041853Z  10SM  CLR").is_ok());
    }

    #[test]
    fn test_highlight_metar() {
        let part = |text: &str, conditions| MetarPart {
            text: text.to_string(),
            conditions,
        };
        let metar = "KDEN 041853Z 22013KT 1 1/2SM BR BKN004 OVC008 OVC020 OVC050 24/07 A3034 RMK VIS 1/2SM BKN002";
        let parts = highlight_metar(metar);
        assert_eq!(
            parts,
            [
                part("KDEN 041853Z 22013KT ", None),
                part("1 1/2SM", Some(WeatherConditions::IFR)),
                part(" BR ", None),
                part("BKN004", Some(WeatherConditions::LIFR)),
                part(" ", None),
                part("OVC008", Some(WeatherConditions::IFR)),
                part(" ", None),
                part("OVC020", Some(WeatherConditions::MVFR)),
                part(" OVC050 24/07 A3034 RMK VIS 1/2SM BKN002", None),
            ]
        );
        let text: String = parts.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(text, metar);

        let clear = "KDEN 041853Z VRB04KT 10SM FEW085 24/07 A3034";
        assert_eq!(highlight_metar(clear), [part(clear, None)]);
        assert_eq!(
            highlight_metar("KXYZ 1/4SM"),
            [
                part("KXYZ ", None),
                part("1/4SM", Some(WeatherConditions::LIFR))
            ]
        );
        assert!(highlight_metar("").is_empty());
    }

    #[test]
    fn test_wind_between() {
        assert!(wind_between(0, 0, 0));
        assert!(wind_between(90, 0, 180));
        assert!(wind_between(5, 350, 10));
        assert!(!wind_between(20, 0, 10));
    }
}
