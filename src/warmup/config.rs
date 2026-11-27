//! `[warmup]` in a profile.

use anyhow::{bail, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// What a warm-up is keyed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Per {
    Repo,
    Project,
}

/// `[warmup]`: opts a profile into a cached base session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// What the warm-up explores.
    pub prompt: String,
    pub per: Per,
    /// The warm-up's turn limit.
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// Its wall-clock limit.
    #[serde(default = "default_timeout")]
    pub timeout: Span,
    /// Bases larger than this are not forked.
    #[serde(default = "default_max_base_tokens")]
    pub max_base_tokens: u64,
    #[serde(default)]
    pub refresh: Refresh,
}

/// `[warmup.refresh]`: any trigger that fires rebuilds the base.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refresh {
    /// Since the base was built.
    #[serde(default = "default_every")]
    pub every: Trigger<Every>,
    /// Default-branch commits since the base.
    #[serde(default = "default_commits")]
    pub commits: Trigger<u64>,
    /// Lines changed (added + deleted) since the base.
    #[serde(default = "default_lines")]
    pub lines: Trigger<u64>,
    /// Limits `commits` and `lines` to these paths; empty is the whole repo.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

impl Default for Refresh {
    fn default() -> Self {
        Refresh {
            every: default_every(),
            commits: default_commits(),
            lines: default_lines(),
            paths: Vec::new(),
        }
    }
}

fn default_max_turns() -> u32 {
    30
}

fn default_timeout() -> Span {
    Span(10 * MINUTE)
}

fn default_max_base_tokens() -> u64 {
    60_000
}

fn default_every() -> Trigger<Every> {
    Trigger::On(Every::Span(Span(DAY)))
}

fn default_commits() -> Trigger<u64> {
    Trigger::On(20)
}

fn default_lines() -> Trigger<u64> {
    Trigger::On(500)
}

/// A refresh trigger: a value, or `false` to turn it off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger<T> {
    Off,
    On(T),
}

impl<T> Trigger<T> {
    pub fn on(&self) -> Option<&T> {
        match self {
            Trigger::On(v) => Some(v),
            Trigger::Off => None,
        }
    }
}

impl<T: Serialize> Serialize for Trigger<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Trigger::Off => s.serialize_bool(false),
            Trigger::On(v) => v.serialize(s),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Trigger<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw<T> {
            Flag(bool),
            Value(T),
        }
        match Raw::<T>::deserialize(d)? {
            Raw::Flag(false) => Ok(Trigger::Off),
            Raw::Flag(true) => Err(serde::de::Error::custom(
                "give a value, or false to turn it off",
            )),
            Raw::Value(v) => Ok(Trigger::On(v)),
        }
    }
}

/// A length of time, in seconds: `30m`, `6h`, `1d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span(pub u64);

impl Span {
    pub fn secs(self) -> u64 {
        self.0
    }
}

impl From<Span> for String {
    fn from(span: Span) -> String {
        match span.0 {
            s if s % DAY == 0 => format!("{}d", s / DAY),
            s if s % HOUR == 0 => format!("{}h", s / HOUR),
            s => format!("{}m", s / MINUTE),
        }
    }
}

impl Serialize for Span {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&String::from(*self))
    }
}

impl<'de> Deserialize<'de> for Span {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        parse_span(&s).map_err(serde::de::Error::custom)
    }
}

const SPAN_FORMS: &str = "minutes (30m), hours (6h) or days (1d)";

fn parse_span(s: &str) -> Result<Span, String> {
    let s = s.trim();
    let (digits, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let per = match unit {
        "m" => MINUTE,
        "h" => HOUR,
        "d" => DAY,
        _ => return Err(format!("{s}: not a duration; give {SPAN_FORMS}")),
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("{s}: not a duration; give {SPAN_FORMS}"))?;
    if n == 0 {
        return Err(format!("{s}: a duration of 0 is no duration"));
    }
    n.checked_mul(per)
        .map(Span)
        .ok_or_else(|| format!("{s}: too large"))
}

/// When `every` fires: a time since the build, or a local time of day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Every {
    Span(Span),
    /// Minutes after local midnight.
    At(u32),
}

impl Serialize for Every {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Every::Span(span) => span.serialize(s),
            Every::At(m) => s.serialize_str(&format!("{:02}:{:02}", m / 60, m % 60)),
        }
    }
}

impl<'de> Deserialize<'de> for Every {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        parse_every(&s).map_err(serde::de::Error::custom)
    }
}

fn parse_every(s: &str) -> Result<Every, String> {
    let s = s.trim();
    let Some((h, m)) = s.split_once(':') else {
        return parse_span(s).map(Every::Span);
    };
    let bad = || format!("{s}: not a time of day; give HH:MM, 00:00 to 23:59");
    let digits = |p: &str| p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit());
    if !digits(h) || !digits(m) {
        return Err(bad());
    }
    let (h, m): (u32, u32) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    if h > 23 || m > 59 {
        return Err(bad());
    }
    Ok(Every::At(h * 60 + m))
}

/// Refuse a `[warmup]` plete cannot honour, as the profile's other checks do.
pub fn check(c: &Config) -> Result<()> {
    if c.prompt.trim().is_empty() {
        bail!("warmup.prompt: say what the warm-up should explore");
    }
    if c.max_turns == 0 {
        bail!("warmup.max_turns = 0 allows nothing; give at least 1");
    }
    if c.max_base_tokens == 0 {
        bail!("warmup.max_base_tokens = 0 allows nothing; give at least 1");
    }
    for path in &c.refresh.paths {
        let p = path.trim();
        if p.is_empty() || p.starts_with('/') || p.split('/').any(|part| part == "..") {
            bail!("warmup.refresh.paths: {path:?} is no path inside the repo");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(toml)
    }

    #[test]
    fn defaults_are_the_specs() {
        let c = parse("prompt = \"map\"\nper = \"repo\"").unwrap();
        assert_eq!(c.per, Per::Repo);
        assert_eq!(c.max_turns, 30);
        assert_eq!(c.timeout, Span(600));
        assert_eq!(c.max_base_tokens, 60_000);
        assert_eq!(c.refresh, Refresh::default());
        assert_eq!(c.refresh.every, Trigger::On(Every::Span(Span(86_400))));
        assert_eq!(c.refresh.commits, Trigger::On(20));
        assert_eq!(c.refresh.lines, Trigger::On(500));
        assert!(c.refresh.paths.is_empty());
        check(&c).unwrap();
    }

    #[test]
    fn each_trigger_can_be_turned_off() {
        let c = parse(
            "prompt = \"m\"\nper = \"project\"\n[refresh]\nevery = false\ncommits = false\nlines = false\npaths = [\"src/\"]",
        )
        .unwrap();
        assert_eq!(c.per, Per::Project);
        assert_eq!(c.refresh.every, Trigger::Off);
        assert_eq!(c.refresh.commits, Trigger::Off);
        assert_eq!(c.refresh.lines, Trigger::Off);
        assert_eq!(c.refresh.paths, ["src/"]);
        // One off leaves the others at their defaults.
        let c = parse("prompt = \"m\"\nper = \"repo\"\n[refresh]\nlines = false").unwrap();
        assert_eq!(c.refresh.lines, Trigger::Off);
        assert_eq!(c.refresh.commits, Trigger::On(20));
        // `true` says nothing.
        assert!(parse("prompt = \"m\"\nper = \"repo\"\n[refresh]\nlines = true").is_err());
    }

    #[test]
    fn every_is_a_duration_or_a_time_of_day() {
        for (text, want) in [
            ("6h", Every::Span(Span(6 * 3600))),
            ("1d", Every::Span(Span(86_400))),
            ("30m", Every::Span(Span(1800))),
            ("06:00", Every::At(360)),
            ("23:59", Every::At(23 * 60 + 59)),
        ] {
            let c = parse(&format!(
                "prompt = \"m\"\nper = \"repo\"\n[refresh]\nevery = \"{text}\""
            ))
            .unwrap();
            assert_eq!(c.refresh.every, Trigger::On(want), "{text}");
        }
        for bad in ["25:00", "06:60", "6:00", "1w", "x", "0h", "", "06:0x"] {
            let r = parse(&format!(
                "prompt = \"m\"\nper = \"repo\"\n[refresh]\nevery = \"{bad}\""
            ));
            assert!(r.is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn timeout_is_a_duration() {
        let c = parse("prompt = \"m\"\nper = \"repo\"\ntimeout = \"15m\"").unwrap();
        assert_eq!(c.timeout, Span(900));
        assert!(parse("prompt = \"m\"\nper = \"repo\"\ntimeout = \"soon\"").is_err());
    }

    #[test]
    fn unknown_keys_and_bad_per_are_refused() {
        assert!(parse("prompt = \"m\"\nper = \"repo\"\nturns = 3").is_err());
        assert!(parse("prompt = \"m\"\nper = \"repo\"\n[refresh]\nweekly = 1").is_err());
        assert!(parse("prompt = \"m\"\nper = \"branch\"").is_err());
        assert!(parse("prompt = \"m\"").is_err(), "per is required");
        assert!(parse("per = \"repo\"").is_err(), "prompt is required");
    }

    #[test]
    fn check_refuses_what_cannot_run() {
        let ok = parse("prompt = \"m\"\nper = \"repo\"").unwrap();
        let mut c = ok.clone();
        c.prompt = "  ".into();
        assert!(check(&c).is_err());
        let mut c = ok.clone();
        c.max_turns = 0;
        assert!(check(&c).is_err());
        for bad in ["", "/etc", "../x", "a/../b"] {
            let mut c = ok.clone();
            c.refresh.paths = vec![bad.into()];
            assert!(check(&c).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn it_serializes_back_to_what_reads() {
        let c = parse(
            "prompt = \"m\"\nper = \"repo\"\n[refresh]\nevery = \"06:00\"\nlines = false\npaths = [\"a\"]",
        )
        .unwrap();
        let text = toml::to_string(&c).unwrap();
        assert_eq!(parse(&text).unwrap(), c);
    }
}
