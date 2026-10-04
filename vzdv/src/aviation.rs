use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

/// The `ceiling` when no layer is broken or overcast.
pub const NO_CEILING: u16 = 3_456;

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

/// Parsed weather information for an airport. Anything the METAR doesn't
/// report is `None`, not assumed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AirportWeather {
    pub name: String,
    /// `None` unless it can be determined from what was reported.
    pub conditions: Option<WeatherConditions>,
    /// Whole statute miles.
    pub visibility: Option<u16>,
    /// Feet, or [`NO_CEILING`]. `None` if the sky condition is missing or the
    /// lowest broken or overcast layer has no height.
    pub ceiling: Option<u16>,
    /// Direction (0 if variable), speed and gust (0 if none).
    pub wind: Option<(u16, u8, u8)>,
    pub altimeter: Option<f32>,
    pub raw: String,
}

/// One group of a METAR: its text as written, and the value to read.
struct Group {
    text: String,
    value: String,
}

/// Split a METAR into groups, rejoining units split off by a stray space
/// ("35005 KT", "2 SM") so each group can be read on its own.
fn metar_groups(metar: &str) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for word in metar.split_whitespace() {
        match groups.last_mut() {
            Some(last)
                if matches!(word, "KT" | "SM")
                    && last.value.ends_with(|c: char| c.is_ascii_digit()) =>
            {
                last.text.push(' ');
                last.text.push_str(word);
                last.value.push_str(word);
            }
            _ => groups.push(Group {
                text: word.to_string(),
                value: word.to_string(),
            }),
        }
    }
    groups
}

/// Index of the remarks, which can repeat cloud and visibility groups in other formats.
fn remarks_start(groups: &[Group]) -> usize {
    groups
        .iter()
        .position(|group| group.value == "RMK")
        .unwrap_or(groups.len())
}

/// Parse a METAR into a struct of data.
pub fn parse_metar(line: &str) -> Result<AirportWeather> {
    let groups = metar_groups(line);
    let station = groups.first().ok_or_else(|| anyhow!("Blank METAR"))?;
    let airport = match station.value.get(1..) {
        Some(faa) if station.value.len() == 4 => faa,
        _ => &station.value,
    };
    // everything after the station identifier, up to the remarks
    let body: Vec<&str> = groups[1..remarks_start(&groups)]
        .iter()
        .map(|group| group.value.as_str())
        .collect();

    let ceiling = match body.iter().find_map(|group| ceiling_layer(group)) {
        // `None` for a layer without a height, like "BKN///"
        Some(height) => height,
        None if body.iter().any(|group| is_sky_condition(group)) => Some(NO_CEILING),
        None => None,
    };
    let visibility = body
        .iter()
        .enumerate()
        .find_map(|(i, group)| visibility_miles(group, i.checked_sub(1).map(|prev| body[prev])));
    let wind = body.iter().find_map(|group| parse_wind(group));
    let conditions = match (
        ceiling.map(WeatherConditions::for_ceiling),
        visibility.map(WeatherConditions::for_visibility),
    ) {
        (Some(ceiling), Some(visibility)) => Some(ceiling.max(visibility)),
        // nothing is worse than LIFR, so the other half doesn't matter
        (Some(WeatherConditions::LIFR), None) | (None, Some(WeatherConditions::LIFR)) => {
            Some(WeatherConditions::LIFR)
        }
        _ => None,
    };
    let altimeter = body.iter().find_map(|group| {
        let hundredths = group.strip_prefix('A')?;
        if hundredths.len() != 4 || !hundredths.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(hundredths.parse::<u16>().ok()? as f32 / 100.0)
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

/// Knots from a speed like "05", "105", or "P99" (over 99).
fn knots(text: &str) -> Option<u8> {
    let digits = text.strip_prefix('P').unwrap_or(text);
    if !(2..=3).contains(&digits.len()) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(digits.parse::<u16>().ok()?.min(u8::MAX.into()) as u8)
}

/// Direction, speed and gust from a wind group like "22013G25KT" or "VRB04KT".
/// Variable winds have a direction of 0. `None` if this isn't a wind group, or
/// the wind is missing ("/////KT").
fn parse_wind(group: &str) -> Option<(u16, u8, u8)> {
    let wind = group.strip_suffix("KT")?;
    let (direction, speeds) = (wind.get(..3)?, wind.get(3..)?);
    let direction = if direction == "VRB" {
        0
    } else if direction.bytes().all(|b| b.is_ascii_digit()) {
        direction.parse().ok()?
    } else {
        return None;
    };
    let (speed, gust) = match speeds.split_once('G') {
        Some((speed, gust)) => (knots(speed)?, knots(gust)?),
        None => (knots(speeds)?, 0),
    };
    Some((direction, speed, gust))
}

/// The 3-character height of a cloud layer like "FEW085", "OVC008CB" or "VV002".
fn layer_height<'a>(group: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    let height = prefixes
        .iter()
        .find_map(|prefix| group.strip_prefix(prefix))?
        .get(..3)?;
    (height == "///" || height.bytes().all(|b| b.is_ascii_digit())).then_some(height)
}

/// Whether a group reports the sky condition: clear skies or a cloud layer.
fn is_sky_condition(group: &str) -> bool {
    matches!(group, "CLR" | "SKC" | "NSC" | "NCD" | "CAVOK")
        || layer_height(group, &["FEW", "SCT", "BKN", "OVC", "VV"]).is_some()
}

/// For a broken, overcast or vertical visibility layer, its height in feet,
/// which is `None` when not reported ("BKN///"). `None` for other groups.
fn ceiling_layer(group: &str) -> Option<Option<u16>> {
    let height = layer_height(group, &["BKN", "OVC", "VV"])?;
    Some(
        height
            .parse::<u16>()
            .ok()
            .map(|hundreds| hundreds.saturating_mul(100)),
    )
}

/// Whole statute miles from a visibility group like "10SM", "1/2SM", "M1/4SM" or "P6SM".
/// `previous` is the group before it, for the "1" in "1 1/2SM".
fn visibility_miles(group: &str, previous: Option<&str>) -> Option<u16> {
    let miles = group.strip_suffix("SM")?;
    let miles = miles
        .strip_prefix('P')
        .or_else(|| miles.strip_prefix('M'))
        .unwrap_or(miles);
    let is_number = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    let Some((numerator, denominator)) = miles.split_once('/') else {
        return is_number(miles).then(|| miles.parse().ok()).flatten();
    };
    if !is_number(numerator) || !is_number(denominator) {
        return None;
    }
    // a fraction is less than a mile, plus any whole miles before it
    Some(
        previous
            .filter(|whole| is_number(whole))
            .and_then(|whole| whole.parse().ok())
            .unwrap_or(0),
    )
}

/// Split a METAR into runs of text, marking the ceilings and visibilities below VFR.
pub fn highlight_metar(metar: &str) -> Vec<MetarPart> {
    let groups = metar_groups(metar);
    let remarks = remarks_start(&groups);
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
        let group = &groups[i].value;
        let next = groups.get(i + 1).filter(|_| i + 1 < remarks);
        // the station identifier and remarks are never highlighted
        let (len, conditions) = if i == 0 || i >= remarks {
            (1, None)
        } else if let Some(miles) = next.and_then(|next| {
            group
                .bytes()
                .all(|b| b.is_ascii_digit())
                .then(|| visibility_miles(&next.value, Some(group)))
                .flatten()
        }) {
            (2, Some(WeatherConditions::for_visibility(miles)))
        } else if let Some(miles) = visibility_miles(group, None) {
            (1, Some(WeatherConditions::for_visibility(miles)))
        } else {
            (
                1,
                ceiling_layer(group)
                    .flatten()
                    .map(WeatherConditions::for_ceiling),
            )
        };
        let text = groups[i..i + len]
            .iter()
            .map(|group| group.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        push(text, conditions.filter(|c| *c != WeatherConditions::VFR));
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
    use super::{
        MetarPart, NO_CEILING, WeatherConditions, highlight_metar, parse_metar, wind_between,
    };
    use WeatherConditions::{IFR, LIFR, MVFR, VFR};

    /// What `parse_metar` should read from a METAR; `None` when it's not reported.
    struct Expected {
        visibility: Option<u16>,
        ceiling: Option<u16>,
        wind: Option<(u16, u8, u8)>,
        altimeter: Option<f32>,
        conditions: Option<WeatherConditions>,
    }

    /// A METAR that reports visibility, sky condition and wind.
    const fn expect(
        visibility: u16,
        ceiling: u16,
        wind: (u16, u8, u8),
        altimeter: Option<f32>,
        conditions: WeatherConditions,
    ) -> Expected {
        Expected {
            visibility: Some(visibility),
            ceiling: Some(ceiling),
            wind: Some(wind),
            altimeter,
            conditions: Some(conditions),
        }
    }

    /// No ceiling layer.
    const CLEAR: u16 = NO_CEILING;

    /// Real METARs unless noted, with what they should parse to.
    const METARS: &[(&str, Expected)] = &[
        // regular reports
        (
            "KDEN 042053Z 01007KT 10SM FEW090 FEW140 27/04 A3029 RMK AO2 SLP195 T02670039 56021",
            expect(10, CLEAR, (10, 7, 0), Some(30.29), VFR),
        ),
        (
            "KTPA 042153Z VRB05KT 10SM FEW039 SCT050 BKN150 BKN300 29/23 A2998 RMK AO2 SLP153 FRQ LTGICCG DSNT N AND SE CB DSNT N AND SE AND NW T02940233 $",
            expect(10, 15_000, (0, 5, 0), Some(29.98), VFR),
        ),
        (
            "KABI 042152Z 01010KT 10SM SCT015 BKN025 OVC050 23/19 A3020 RMK AO2 SLP208 T02280189 $",
            expect(10, 2_500, (10, 10, 0), Some(30.20), MVFR),
        ),
        (
            "KRDU 042151Z 35005KT 10SM OVC008 18/17 A3008 RMK AO2 SLP182 T01830167 $",
            expect(10, 800, (350, 5, 0), Some(30.08), IFR),
        ),
        (
            "KCOS 042154Z 16014G26KT 10SM FEW120 FEW200 22/08 A3039 RMK AO2 PK WND 15026/2150 SLP227 T02220078",
            expect(10, CLEAR, (160, 14, 26), Some(30.39), VFR),
        ),
        // corrected and automated reports
        (
            "KPTK 042153Z COR 28011KT 10SM FEW055 21/10 A3005 RMK AO2 SLP174 T02060100",
            expect(10, CLEAR, (280, 11, 0), Some(30.05), VFR),
        ),
        (
            "KFLP 042156Z AUTO 00000KT 10SM CLR 27/09 A3015 RMK AO2 SLP204 T02720089",
            expect(10, CLEAR, (0, 0, 0), Some(30.15), VFR),
        ),
        (
            "KIXA 042135Z AUTO 35005KT 3SM -DZ BKN005 OVC011 18/18 A3005 RMK AO2",
            expect(3, 500, (350, 5, 0), Some(30.05), IFR),
        ),
        // whole and mixed-fraction visibilities
        (
            "KMLJ 042135Z AUTO 00000KT 5SM -RA BR SCT006 SCT024 BKN055 23/23 A3005 RMK AO2",
            expect(5, 5_500, (0, 0, 0), Some(30.05), MVFR),
        ),
        (
            "KAZE 251935Z AUTO 36009G16KT 320V040 1 1/2SM +RA SCT019 BKN031 OVC038 27/27 A3007 RMK AO2 LTG DSNT SE THRU SW P0023 T02720267",
            expect(1, 3_100, (360, 9, 16), Some(30.07), IFR),
        ),
        (
            "KNPA 042156Z 04005KT 2 1/2SM RA BR SCT019 BKN042 OVC050 24/24 A2997 RMK AO2 SLP149 T02440239 PNO $",
            expect(2, 4_200, (40, 5, 0), Some(29.97), IFR),
        ),
        (
            "K2J5 061830Z AUTO 03019G36KT 350V050 1 1/4SM +TSRA BKN008 BKN024 OVC047 25/20 A2985 RMK AO2 LTG DSNT ALQDS P0091 T02470201",
            expect(1, 800, (30, 19, 36), Some(29.85), IFR),
        ),
        (
            "KUKL 042155Z AUTO 01010G14KT 1 3/4SM HZ VV007 26/10 A3025 RMK AO2 T02550096",
            expect(1, 700, (10, 10, 14), Some(30.25), IFR),
        ),
        (
            "KN38 042155Z AUTO 20006KT 1SM BR OVC004 13/13 A3009 RMK AO2",
            expect(1, 400, (200, 6, 0), Some(30.09), LIFR),
        ),
        (
            "KGIC 042135Z AUTO 07007KT 3/4SM VV005 27/07 A3008 RMK AO1",
            expect(0, 500, (70, 7, 0), Some(30.08), LIFR),
        ),
        (
            "KMEI 042147Z 00000KT 1/4SM R01/2400V4000FT +RA FG BKN017 OVC021 23/23 A3001 RMK AO2 TSB10E25 P0077 T02330228",
            expect(0, 1_700, (0, 0, 0), Some(30.01), LIFR),
        ),
        (
            "KHSP 042155Z AUTO 27010KT M1/4SM FG 14/14 A3017 RMK AO2 T01420140",
            // no sky condition, but the visibility alone makes it LIFR
            Expected {
                visibility: Some(0),
                ceiling: None,
                wind: Some((270, 10, 0)),
                altimeter: Some(30.17),
                conditions: Some(LIFR),
            },
        ),
        (
            "KOTM 131356Z AUTO 33029G39KT 3/4SM +TSRA BR BKN003 OVC008 21/21 A2992 RMK AO2 PK WND 31039/1354 LTG DSNT ALQDS PRESRR P0019 T02110211",
            expect(0, 300, (330, 29, 39), Some(29.92), LIFR),
        ),
        // KDEN's 1453Z report on 2026-09-15 split its wind group, which used to
        // fail the KDEN page with "invalid digit found in string"
        (
            "KDEN 151453Z 35005 KT 10SM BKN037 BKN080 14/01 A3032 RMK AO2 SLP203 T01390006 51020",
            expect(10, 3_700, (350, 5, 0), Some(30.32), VFR),
        ),
        // missing groups make whatever depends on them unknown
        (
            "K5SM 042211Z AUTO 10SM CLR 15/M04 A3070 RMK AO2",
            Expected {
                visibility: Some(10),
                ceiling: Some(CLEAR),
                wind: None,
                altimeter: Some(30.70),
                conditions: Some(VFR),
            },
        ),
        (
            // clear skies say nothing about the visibility
            "KAJZ 042215Z AUTO 27009KT CLR 29/03 A3028 RMK AO2 T02940034 PWINO",
            Expected {
                visibility: None,
                ceiling: Some(CLEAR),
                wind: Some((270, 9, 0)),
                altimeter: Some(30.28),
                conditions: None,
            },
        ),
        (
            "KCPW 070435Z AUTO 03007KT OVC003 M13/M15 A3013 RMK AO2 PWINO",
            // nothing is worse than LIFR, so the missing visibility doesn't matter
            Expected {
                visibility: None,
                ceiling: Some(300),
                wind: Some((30, 7, 0)),
                altimeter: Some(30.13),
                conditions: Some(LIFR),
            },
        ),
        (
            "K4BM 070435Z AUTO 36006KT BKN009 OVC014 A3021 RMK AO2 PWINO",
            // at least IFR, but it could be LIFR
            Expected {
                visibility: None,
                ceiling: Some(900),
                wind: Some((360, 6, 0)),
                altimeter: Some(30.21),
                conditions: None,
            },
        ),
        (
            "K40B 042120Z AUTO 16007G16KT 090V190 13/02 RMK AO1 SLPNO T01350025",
            Expected {
                visibility: None,
                ceiling: None,
                wind: Some((160, 7, 16)),
                altimeter: None,
                conditions: None,
            },
        ),
        (
            "KAEJ 242115Z AUTO 18/M10 A3011 RMK AO2 T01801100 PWINO",
            Expected {
                visibility: None,
                ceiling: None,
                wind: None,
                altimeter: Some(30.11),
                conditions: None,
            },
        ),
        // synthetic: the same stray space before the visibility's units
        (
            "KXYZ 041856Z 18010KT 2 SM BR OVC006 15/14 A3001",
            expect(2, 600, (180, 10, 0), Some(30.01), IFR),
        ),
        // synthetic: an automated station's missing wind and lowest cloud height
        (
            "KXYZ 041856Z AUTO /////KT 10SM BKN/// OVC045 15/05 A3001 RMK AO2 PWINO",
            Expected {
                visibility: Some(10),
                ceiling: None,
                wind: None,
                altimeter: Some(30.01),
                conditions: None,
            },
        ),
        // synthetic: 3-digit winds, ICAO's "P99" for gusts over 99 kt, and a CB layer
        (
            "KXYZ 041856Z 270105G120KT 1SM +SN BLSN OVC010CB M10/M12 A2950",
            expect(1, 1_000, (270, 105, 120), Some(29.50), IFR),
        ),
        (
            "KXYZ 041856Z 27050GP99KT P6SM SCT030TCU BKN050 15/05 A2950",
            expect(6, 5_000, (270, 50, 99), Some(29.50), VFR),
        ),
        // synthetic: remarks restate visibility and ceilings; only the body counts
        (
            "KXYZ 041856Z 22013KT 10SM BKN045 24/07 A3034 RMK CIG 005V010 VIS 1/2SM BKN002",
            expect(10, 4_500, (220, 13, 0), Some(30.34), VFR),
        ),
        // international formats have no statute-mile visibility or inHg altimeter
        (
            "EGLL 041850Z AUTO 29005KT 9999 NCD 19/09 Q1027",
            Expected {
                visibility: None,
                ceiling: Some(CLEAR),
                wind: Some((290, 5, 0)),
                altimeter: None,
                conditions: None,
            },
        ),
    ];

    #[test]
    fn test_parse_metar_examples() {
        for (metar, expected) in METARS {
            let ret = parse_metar(metar).unwrap();
            assert_eq!(ret.visibility, expected.visibility, "visibility: {metar}");
            assert_eq!(ret.ceiling, expected.ceiling, "ceiling: {metar}");
            assert_eq!(ret.wind, expected.wind, "wind: {metar}");
            assert_eq!(ret.altimeter, expected.altimeter, "altimeter: {metar}");
            assert_eq!(ret.conditions, expected.conditions, "conditions: {metar}");
            assert_eq!(ret.raw, *metar);
        }
    }

    #[test]
    fn test_highlight_metar_examples() {
        for (metar, expected) in METARS {
            let parts = highlight_metar(metar);
            // the text is unchanged
            let text: String = parts.iter().map(|part| part.text.as_str()).collect();
            assert_eq!(text, *metar, "{metar}");
            // runs alternate, and only below-VFR groups are marked
            assert!(
                parts
                    .windows(2)
                    .all(|pair| pair[0].conditions.is_some() || pair[1].conditions.is_some()),
                "{metar}"
            );
            assert!(parts.iter().all(|part| part.conditions != Some(VFR)));
            // with both halves reported, the worst highlight matches the conditions
            let worst = parts.iter().filter_map(|part| part.conditions).max();
            if expected.visibility.is_some() && expected.ceiling.is_some() {
                assert_eq!(
                    Some(worst.unwrap_or(VFR)),
                    expected.conditions,
                    "conditions: {metar}"
                );
            }
        }
    }

    /// The highlighted groups of a METAR.
    fn highlights(metar: &str) -> Vec<(String, WeatherConditions)> {
        highlight_metar(metar)
            .into_iter()
            .filter_map(|part| Some((part.text, part.conditions?)))
            .collect()
    }

    #[test]
    fn test_highlight_metar_groups() {
        let marked = |pairs: &[(&str, WeatherConditions)]| -> Vec<(String, WeatherConditions)> {
            pairs
                .iter()
                .map(|(text, c)| (text.to_string(), *c))
                .collect()
        };
        assert_eq!(
            highlights("KIXA 042135Z AUTO 35005KT 3SM -DZ BKN005 OVC011 18/18 A3005 RMK AO2"),
            marked(&[("3SM", MVFR), ("BKN005", IFR), ("OVC011", MVFR)])
        );
        assert_eq!(
            highlights(
                "KUKL 042155Z AUTO 01010G14KT 1 3/4SM HZ VV007 26/10 A3025 RMK AO2 T02550096"
            ),
            marked(&[("1 3/4SM", IFR), ("VV007", IFR)])
        );
        assert_eq!(
            highlights("KHSP 042155Z AUTO 27010KT M1/4SM FG 14/14 A3017 RMK AO2 T01420140"),
            marked(&[("M1/4SM", LIFR)])
        );
        // RVR isn't a visibility, and SCT layers aren't ceilings
        assert_eq!(
            highlights(
                "KMEI 042147Z 00000KT 1/4SM R01/2400V4000FT +RA FG SCT005 BKN017 OVC021 23/23 A3001"
            ),
            marked(&[("1/4SM", LIFR), ("BKN017", MVFR), ("OVC021", MVFR)])
        );
        assert_eq!(
            highlights("KXYZ 041856Z 18010KT 2 SM BR OVC006 15/14 A3001"),
            marked(&[("2 SM", IFR), ("OVC006", IFR)])
        );
        // the station identifier, VFR groups and remarks stay plain
        assert!(highlights("K5SM 070435Z AUTO 10SM CLR M12/M13 A3015 RMK AO2").is_empty());
        assert!(
            highlights("KDEN 151453Z 35005 KT 10SM BKN037 BKN080 14/01 A3032 RMK AO2 SLP203 T01390006 51020")
                .is_empty()
        );
        assert!(
            highlights(
                "KXYZ 041856Z 22013KT 10SM BKN045 24/07 A3034 RMK CIG 005V010 VIS 1/2SM BKN002"
            )
            .is_empty()
        );
        assert!(highlights("VVNB 041830Z 09004KT 9999 FEW020 BKN040 31/25 Q1006").is_empty());
    }

    #[test]
    fn test_parse_metar_blank_and_messy_input() {
        assert!(parse_metar("").is_err());
        assert!(parse_metar("   ").is_err());
        let ret = parse_metar("KDEN  041853Z  22013KT   10SM CLR 24/07 A3034").unwrap();
        assert_eq!(ret.name, "DEN");
        assert_eq!(ret.wind, Some((220, 13, 0)));
        assert_eq!(ret.visibility, Some(10));
        // non-ASCII doesn't panic
        assert!(parse_metar("ÉDEN 041853Z 22013KT 10SM").is_ok());
    }

    #[test]
    fn test_parse_metar_standard() {
        let ret = parse_metar("KDEN 030253Z 22013KT 10SM SCT100 BKN160 13/M12 A2943 RMK AO2 PK WND 21036/0211 SLP924 T01331117 58005").unwrap();
        assert_eq!(ret.name, "DEN");
        assert_eq!(ret.conditions, Some(VFR));

        let ret = parse_metar("KDEN 2SM BKN005").unwrap();
        assert_eq!(ret.conditions, Some(IFR));

        let ret = parse_metar("KDEN 4SM OVC020").unwrap();
        assert_eq!(ret.conditions, Some(MVFR));

        let ret = parse_metar("KDEN 1/2SM OVC001").unwrap();
        assert_eq!(ret.conditions, Some(LIFR));

        // a misspelled layer isn't a sky condition, so an IFR visibility can't settle it
        let ret = parse_metar("KDEN 2SM BNK005").unwrap();
        assert_eq!(ret.ceiling, None);
        assert_eq!(ret.conditions, None);
    }

    #[test]
    fn test_parse_metar_wind() {
        // missing isn't calm
        let missing = "K5SM 070435Z AUTO 10SM CLR M12/M13 A3015 RMK AO2";
        let ret = parse_metar(missing).unwrap();
        assert_eq!(ret.wind, None);
        let calm = "KFLP 042156Z AUTO 00000KT 10SM CLR 27/09 A3015 RMK AO2 SLP204 T02720089";
        assert_eq!(parse_metar(calm).unwrap().wind, Some((0, 0, 0)));

        let standard = "KCPW 070435Z AUTO 03007KT OVC003 M13/M15 A3013 RMK AO2 PWINO";
        let ret = parse_metar(standard).unwrap();
        assert_eq!(ret.wind, Some((30, 7, 0)));

        let gust = "KFLY 070435Z AUTO 36014G21KT OVC036 M05/M07 A3028 RMK AO2 T10531075 PWINO";
        let ret = parse_metar(gust).unwrap();
        assert_eq!(ret.wind, Some((360, 14, 21)));
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
        assert_eq!(conditions("KDEN 10SM OVC010"), Some(MVFR));
        assert_eq!(conditions("KDEN 10SM OVC009"), Some(IFR));
        assert_eq!(conditions("KDEN 10SM BKN005"), Some(IFR));
        assert_eq!(conditions("KDEN 10SM BKN004"), Some(LIFR));
        assert_eq!(conditions("KDEN 10SM OVC030"), Some(MVFR));
        assert_eq!(conditions("KDEN 10SM OVC031"), Some(VFR));
        assert_eq!(conditions("KDEN 5SM CLR"), Some(MVFR));
        assert_eq!(conditions("KDEN 3SM CLR"), Some(MVFR));
        // FEW and SCT layers report the sky but aren't ceilings
        assert_eq!(conditions("KDEN 10SM FEW005 SCT008"), Some(VFR));
        // the worse of the two wins
        assert_eq!(conditions("KDEN 2SM OVC050"), Some(IFR));
        assert_eq!(conditions("KDEN 10SM VV002"), Some(LIFR));

        let ret = parse_metar("KDEN 041853Z 1 1/2SM BR OVC008CB 24/07 A3034").unwrap();
        assert_eq!(ret.visibility, Some(1));
        assert_eq!(ret.ceiling, Some(800));
        assert_eq!(ret.conditions, Some(IFR));
        assert_eq!(parse_metar("KDEN M1/4SM FG").unwrap().visibility, Some(0));
        assert_eq!(parse_metar("KDEN P6SM SKC").unwrap().visibility, Some(6));

        // the lowest layer's height is missing, so the ceiling is unknown
        let ret = parse_metar("KDEN 041853Z AUTO 10SM BKN/// OVC045 A3034").unwrap();
        assert_eq!(ret.ceiling, None);
        assert_eq!(ret.conditions, None);
        // a missing height above a known ceiling doesn't matter
        let ret = parse_metar("KDEN 041853Z AUTO 10SM OVC045 OVC/// A3034").unwrap();
        assert_eq!(ret.ceiling, Some(4_500));
        assert_eq!(ret.conditions, Some(VFR));
        // remarks are ignored
        let ret = parse_metar("KDEN 041853Z 10SM OVC045 A3034 RMK CIG 005V010 VIS 1/2SM").unwrap();
        assert_eq!(ret.ceiling, Some(4_500));
        assert_eq!(ret.visibility, Some(10));
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
