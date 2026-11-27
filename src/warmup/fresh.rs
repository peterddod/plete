//! When a base is stale. Pure but for the [`GitView`] it asks.

use std::fmt;

use super::cache::Meta;
use super::config::{Config, Every};

/// The git facts freshness needs, per repo (as keyed in `Meta::commits`),
/// against the freshly fetched default branch. `None`: git could not say, as
/// when the base's commit is gone.
pub trait GitView {
    /// Commits past `base`, limited to `paths` if any.
    fn commits_since(&self, repo: &str, base: &str, paths: &[String]) -> Option<u64>;
    /// Lines added plus deleted since `base`, limited to `paths` if any.
    fn lines_since(&self, repo: &str, base: &str, paths: &[String]) -> Option<u64>;
    /// Files that differ between `base` and the default branch.
    fn changed_files(&self, repo: &str, base: &str) -> Option<Vec<String>>;
}

/// Why a base is stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// No base, or its meta is missing or unreadable.
    NoBase,
    /// The prompt, model or harness version changed.
    Fingerprint,
    Every,
    Commits(u64),
    Lines(u64),
    /// A file the warm-up read changed.
    FileChanged(String),
    /// Git could not compare the base with the default branch.
    Unknown(String),
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Why::NoBase => write!(f, "no base"),
            Why::Fingerprint => write!(f, "profile, model or harness changed"),
            Why::Every => write!(f, "refresh schedule passed"),
            Why::Commits(n) => write!(f, "{n} commits since"),
            Why::Lines(n) => write!(f, "{n} lines changed since"),
            Why::FileChanged(p) => write!(f, "{p} changed"),
            Why::Unknown(repo) => write!(f, "cannot compare {repo} with its default branch"),
        }
    }
}

/// `None` when `meta` is fresh, else why not. `now` is unix seconds and
/// `utc_offset` the local zone's offset in seconds, for `every` as a time of
/// day. `fingerprint` is [`fingerprint`] of today's profile.
pub fn staleness(
    meta: Option<&Meta>,
    cfg: &Config,
    fingerprint: &str,
    now: i64,
    utc_offset: i64,
    git: &dyn GitView,
) -> Option<Why> {
    let Some(meta) = meta else {
        return Some(Why::NoBase);
    };
    if meta.fingerprint != fingerprint {
        return Some(Why::Fingerprint);
    }
    if let Some(every) = cfg.refresh.every.on() {
        if every_fired(*every, meta.built_at, now, utc_offset) {
            return Some(Why::Every);
        }
    }
    let paths = &cfg.refresh.paths;
    let sum = |ask: &dyn Fn(&str, &str) -> Option<u64>| -> Result<u64, Why> {
        meta.commits
            .iter()
            .map(|(repo, base)| ask(repo, base).ok_or_else(|| Why::Unknown(repo.clone())))
            .sum()
    };
    if let Some(limit) = cfg.refresh.commits.on() {
        match sum(&|r, b| git.commits_since(r, b, paths)) {
            Ok(n) if n > *limit => return Some(Why::Commits(n)),
            Ok(_) => {}
            Err(why) => return Some(why),
        }
    }
    if let Some(limit) = cfg.refresh.lines.on() {
        match sum(&|r, b| git.lines_since(r, b, paths)) {
            Ok(n) if n > *limit => return Some(Why::Lines(n)),
            Ok(_) => {}
            Err(why) => return Some(why),
        }
    }
    // A harness that cannot report reads leaves this rule out.
    if !meta.files_read.is_empty() {
        for (repo, base) in &meta.commits {
            let Some(changed) = git.changed_files(repo, base) else {
                return Some(Why::Unknown(repo.clone()));
            };
            let hit = meta.files_read.iter().find(|read| {
                read.strip_prefix(repo.as_str())
                    .and_then(|rest| rest.strip_prefix('/'))
                    .is_some_and(|rel| changed.iter().any(|c| c == rel))
            });
            if let Some(read) = hit {
                return Some(Why::FileChanged(read.clone()));
            }
        }
    }
    None
}

fn every_fired(every: Every, built_at: i64, now: i64, utc_offset: i64) -> bool {
    const DAY: i64 = 86_400;
    match every {
        Every::Span(span) => now.saturating_sub(built_at) >= span.secs() as i64,
        Every::At(minutes) => {
            // The latest local `HH:MM` at or before now; stale once it falls
            // after the build.
            let local = now + utc_offset;
            let mut at = local - local.rem_euclid(DAY) + i64::from(minutes) * 60 - utc_offset;
            if at > now {
                at -= DAY;
            }
            at > built_at
        }
    }
}

/// What a base was built under: the `[warmup]` section without
/// `[warmup.refresh]`, the model and the harness version. A change to a
/// threshold alone leaves it alone.
pub fn fingerprint(cfg: &Config, model: Option<&str>, harness_version: &str) -> String {
    let mut table = toml::Table::try_from(cfg).expect("a warmup config serializes");
    table.remove("refresh");
    let text = format!("{table}\0{}\0{harness_version}", model.unwrap_or_default());
    format!("{:016x}", fnv1a(text.as_bytes()))
}

/// FNV-1a: stable across Rust versions, unlike `DefaultHasher`.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::warmup::config::{Refresh, Span, Trigger};
    use std::collections::BTreeMap;

    const T0: i64 = 1_700_000_000;

    /// A fixed answer per repo, `r`.
    #[derive(Default)]
    struct Fake {
        commits: Option<u64>,
        lines: Option<u64>,
        changed: Vec<String>,
        /// What `paths` were asked for.
        seen: std::cell::RefCell<Vec<Vec<String>>>,
        /// Counts to give when `paths` is `["src/"]`.
        scoped: Option<(u64, u64)>,
    }

    impl Fake {
        fn at(commits: u64, lines: u64) -> Fake {
            Fake {
                commits: Some(commits),
                lines: Some(lines),
                ..Default::default()
            }
        }
    }

    impl GitView for Fake {
        fn commits_since(&self, _: &str, _: &str, paths: &[String]) -> Option<u64> {
            self.seen.borrow_mut().push(paths.to_vec());
            match (paths.is_empty(), self.scoped) {
                (false, Some((c, _))) => Some(c),
                _ => self.commits,
            }
        }
        fn lines_since(&self, _: &str, _: &str, paths: &[String]) -> Option<u64> {
            match (paths.is_empty(), self.scoped) {
                (false, Some((_, l))) => Some(l),
                _ => self.lines,
            }
        }
        fn changed_files(&self, _: &str, _: &str) -> Option<Vec<String>> {
            Some(self.changed.clone())
        }
    }

    fn cfg() -> Config {
        toml::from_str("prompt = \"m\"\nper = \"repo\"").unwrap()
    }

    fn meta() -> Meta {
        Meta {
            base_session: "b".into(),
            built_at: T0,
            commits: BTreeMap::from([("r".into(), "base".into())]),
            fingerprint: "fp".into(),
            files_read: vec![],
            turns: 1,
            duration_secs: 1,
            base_tokens: Some(1),
        }
    }

    fn stale(meta: &Meta, cfg: &Config, now: i64, git: &Fake) -> Option<Why> {
        staleness(Some(meta), cfg, "fp", now, 0, git)
    }

    #[test]
    fn a_quiet_base_is_fresh() {
        assert_eq!(stale(&meta(), &cfg(), T0 + 3600, &Fake::at(0, 0)), None);
    }

    #[test]
    fn no_meta_is_stale_not_an_error() {
        assert_eq!(
            staleness(None, &cfg(), "fp", T0, 0, &Fake::at(0, 0)),
            Some(Why::NoBase)
        );
    }

    #[test]
    fn every_as_a_duration() {
        let mut c = cfg();
        c.refresh.every = Trigger::On(Every::Span(Span(6 * 3600)));
        let g = Fake::at(0, 0);
        assert_eq!(stale(&meta(), &c, T0 + 6 * 3600 - 1, &g), None);
        assert_eq!(stale(&meta(), &c, T0 + 6 * 3600, &g), Some(Why::Every));
        // Default: a day.
        assert_eq!(stale(&meta(), &cfg(), T0 + 86_399, &g), None);
        assert_eq!(stale(&meta(), &cfg(), T0 + 86_400, &g), Some(Why::Every));
    }

    #[test]
    fn every_as_a_time_of_day() {
        let mut c = cfg();
        c.refresh.every = Trigger::On(Every::At(6 * 60));
        let g = Fake::at(0, 0);
        // 2023-11-14 00:00:00 UTC.
        let day = 1_699_920_000;
        let at = |t: i64| Meta {
            built_at: t,
            ..meta()
        };
        // Built 05:59, stale at 06:00 but not before.
        assert_eq!(
            stale(
                &at(day + 5 * 3600 + 59 * 60),
                &c,
                day + 5 * 3600 + 59 * 60 + 30,
                &g
            ),
            None
        );
        assert_eq!(
            stale(&at(day + 5 * 3600 + 59 * 60), &c, day + 6 * 3600, &g),
            Some(Why::Every)
        );
        // Built 06:01, fresh until the next 06:00.
        assert_eq!(
            stale(&at(day + 6 * 3600 + 60), &c, day + 23 * 3600, &g),
            None
        );
        assert_eq!(
            stale(&at(day + 6 * 3600 + 60), &c, day + 86_400 + 5 * 3600, &g),
            None
        );
        assert_eq!(
            stale(&at(day + 6 * 3600 + 60), &c, day + 86_400 + 6 * 3600, &g),
            Some(Why::Every)
        );
    }

    #[test]
    fn a_time_of_day_is_local() {
        // UTC+2: 06:00 local is 04:00 UTC.
        let mut c = cfg();
        c.refresh.every = Trigger::On(Every::At(6 * 60));
        let g = Fake::at(0, 0);
        let day = 1_699_920_000;
        let m = Meta {
            built_at: day + 3 * 3600,
            ..meta()
        }; // 05:00 local
        let at = |now| staleness(Some(&m), &c, "fp", now, 2 * 3600, &g);
        assert_eq!(at(day + 3 * 3600 + 3000), None); // 05:50 local
        assert_eq!(at(day + 4 * 3600), Some(Why::Every)); // 06:00 local
                                                          // West of UTC across midnight: UTC-5, 06:00 local is 11:00 UTC.
        let m = Meta {
            built_at: day + 10 * 3600,
            ..meta()
        }; // 05:00 local
        assert_eq!(
            staleness(Some(&m), &c, "fp", day + 11 * 3600, -5 * 3600, &g),
            Some(Why::Every)
        );
        assert_eq!(
            staleness(Some(&m), &c, "fp", day + 10 * 3600 + 1800, -5 * 3600, &g),
            None
        );
    }

    #[test]
    fn commits_trigger() {
        let g = |n| Fake::at(n, 0);
        assert_eq!(stale(&meta(), &cfg(), T0, &g(20)), None);
        assert_eq!(stale(&meta(), &cfg(), T0, &g(21)), Some(Why::Commits(21)));
    }

    #[test]
    fn lines_trigger() {
        let g = |n| Fake::at(0, n);
        assert_eq!(stale(&meta(), &cfg(), T0, &g(500)), None);
        assert_eq!(stale(&meta(), &cfg(), T0, &g(501)), Some(Why::Lines(501)));
    }

    #[test]
    fn paths_limit_commits_and_lines() {
        // Plenty overall, little under src/.
        let g = Fake {
            scoped: Some((3, 40)),
            ..Fake::at(100, 9000)
        };
        assert!(stale(&meta(), &cfg(), T0, &g).is_some());
        let mut c = cfg();
        c.refresh.paths = vec!["src/".into()];
        assert_eq!(stale(&meta(), &c, T0, &g), None);
        assert!(g.seen.borrow().iter().any(|p| p == &["src/".to_string()]));
        // And enough under src/ still fires.
        let g = Fake {
            scoped: Some((21, 0)),
            ..Fake::at(0, 0)
        };
        assert_eq!(stale(&meta(), &c, T0, &g), Some(Why::Commits(21)));
        let g = Fake {
            scoped: Some((0, 501)),
            ..Fake::at(0, 0)
        };
        assert_eq!(stale(&meta(), &c, T0, &g), Some(Why::Lines(501)));
    }

    #[test]
    fn a_trigger_can_be_off() {
        let mut c = cfg();
        c.refresh.commits = Trigger::Off;
        assert_eq!(stale(&meta(), &c, T0, &Fake::at(999, 0)), None);
        let mut c = cfg();
        c.refresh.lines = Trigger::Off;
        assert_eq!(stale(&meta(), &c, T0, &Fake::at(0, 99_999)), None);
        let mut c = cfg();
        c.refresh.every = Trigger::Off;
        assert_eq!(stale(&meta(), &c, T0 + 10_000_000, &Fake::at(0, 0)), None);
    }

    #[test]
    fn all_triggers_off_is_never_stale_by_trigger() {
        let mut c = cfg();
        c.refresh = Refresh {
            every: Trigger::Off,
            commits: Trigger::Off,
            lines: Trigger::Off,
            paths: vec![],
        };
        assert_eq!(
            stale(&meta(), &c, T0 + 10_000_000, &Fake::at(999, 99_999)),
            None
        );
        // The always-rules still apply.
        let m = Meta {
            files_read: vec!["r/a.rs".into()],
            ..meta()
        };
        let g = Fake {
            changed: vec!["a.rs".into()],
            ..Fake::at(0, 0)
        };
        assert!(stale(&m, &c, T0, &g).is_some());
        assert_eq!(
            staleness(Some(&meta()), &c, "other", T0, 0, &g),
            Some(Why::Fingerprint)
        );
    }

    #[test]
    fn a_changed_read_file_is_stale() {
        let m = Meta {
            files_read: vec!["r/src/a.rs".into()],
            ..meta()
        };
        let g = Fake {
            changed: vec!["src/b.rs".into()],
            ..Fake::at(0, 0)
        };
        assert_eq!(stale(&m, &cfg(), T0, &g), None);
        let g = Fake {
            changed: vec!["src/a.rs".into()],
            ..Fake::at(0, 0)
        };
        assert_eq!(
            stale(&m, &cfg(), T0, &g),
            Some(Why::FileChanged("r/src/a.rs".into()))
        );
        // Another repo's file of the same name does not count.
        let m = Meta {
            files_read: vec!["rr/src/a.rs".into()],
            ..meta()
        };
        assert_eq!(stale(&m, &cfg(), T0, &g), None);
    }

    #[test]
    fn no_files_read_skips_that_rule() {
        let g = Fake {
            changed: vec!["src/a.rs".into()],
            ..Fake::at(0, 0)
        };
        assert_eq!(stale(&meta(), &cfg(), T0, &g), None);
    }

    #[test]
    fn a_project_sums_its_repos() {
        let mut m = meta();
        m.commits.insert("s".into(), "base2".into());
        // 12 per repo: under 20 each, over together.
        assert_eq!(
            stale(&m, &cfg(), T0, &Fake::at(12, 0)),
            Some(Why::Commits(24))
        );
        assert_eq!(stale(&m, &cfg(), T0, &Fake::at(10, 0)), None);
    }

    #[test]
    fn git_unable_to_answer_is_stale() {
        let g = Fake::default();
        assert_eq!(
            stale(&meta(), &cfg(), T0, &g),
            Some(Why::Unknown("r".into()))
        );
    }

    #[test]
    fn the_fingerprint_follows_prompt_model_and_harness_but_not_thresholds() {
        let c = cfg();
        let base = fingerprint(&c, Some("opus"), "2.1.327");
        assert_eq!(base, fingerprint(&c, Some("opus"), "2.1.327"));
        assert_ne!(base, fingerprint(&c, Some("sonnet"), "2.1.327"));
        assert_ne!(base, fingerprint(&c, None, "2.1.327"));
        assert_ne!(base, fingerprint(&c, Some("opus"), "2.1.328"));
        let mut edited = c.clone();
        edited.prompt = "other".into();
        assert_ne!(base, fingerprint(&edited, Some("opus"), "2.1.327"));
        let mut per = c.clone();
        per.per = crate::warmup::config::Per::Project;
        assert_ne!(base, fingerprint(&per, Some("opus"), "2.1.327"));
        // A refresh-only edit leaves it alone.
        let mut r = c.clone();
        r.refresh.commits = Trigger::On(5);
        r.refresh.every = Trigger::Off;
        r.refresh.paths = vec!["src/".into()];
        assert_eq!(base, fingerprint(&r, Some("opus"), "2.1.327"));
        // And a stale fingerprint makes the base stale.
        assert_eq!(
            staleness(Some(&meta()), &c, "else", T0, 0, &Fake::at(0, 0)),
            Some(Why::Fingerprint)
        );
    }
}
