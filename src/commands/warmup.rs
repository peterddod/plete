//! `plete warmup`: the session warm-ups kept for agent profiles.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::agent::{age_label, AgentSpec};
use crate::context::App;
use crate::git;
use crate::profiles::{self, Profile};
use crate::projects;
use crate::warmup::build::{self, Ensured};
use crate::warmup::cache::{self, Meta};
use crate::warmup::config::Config;
use crate::warmup::{lock, root};

/// Every warm-up kept, as (profile, scope), sorted.
pub fn kept(app: &App) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for profile in subdirs(&root(&app.home)) {
        for scope in subdirs(&root(&app.home).join(&profile)) {
            out.push((profile.clone(), scope));
        }
    }
    out.sort();
    out
}

/// The directories in `dir` that are a name of ours: not `.slots`.
fn subdirs(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| cache::safe_name(n).is_ok())
        .collect()
}

/// The profile `name`, as a task in project `scope` sees it if there is one,
/// and its `[warmup]`.
fn warmup_profile(app: &App, name: &str, scope: Option<&str>) -> Result<(Profile, Config)> {
    let project = scope.filter(|s| projects::existing_dir(app, s).is_ok());
    let profile = match project.map(|p| profiles::find(app, name, Some(p))) {
        Some(Ok(profile)) => profile,
        _ => profiles::find(app, name, None)?,
    };
    let Some(cfg) = profile.settings.warmup.clone() else {
        bail!("profile {name} has no [warmup]; `plete agents show {name}` shows it");
    };
    Ok((profile, cfg))
}

/// The status of the base kept in `dir`, by the default branch as last fetched.
fn status(app: &App, name: &str, scope: &str, dir: &Path) -> String {
    if lock::is_held(&lock::key_lock(dir)) {
        return "building".into();
    }
    let checked = || -> Result<String> {
        let (profile, cfg) = warmup_profile(app, name, Some(scope))?;
        let spec = AgentSpec::from_config(&app.config)?;
        let print = build::fingerprint(&spec, &cfg, build::model_of(app, &profile));
        let source = build::source_for(app, cfg.per, scope)?;
        Ok(
            match build::stale(dir, &cfg, &print, &build::tips(&source)) {
                None => "fresh".into(),
                Some(why) => format!("stale: {why}"),
            },
        )
    };
    checked().unwrap_or_else(|e| format!("unknown: {e:#}"))
}

fn tokens(n: Option<u64>) -> String {
    match n {
        None => "-".into(),
        Some(n) if n >= 1000 => format!("{:.1}k", n as f64 / 1000.0),
        Some(n) => n.to_string(),
    }
}

fn commit_label(meta: &Meta) -> String {
    let short = |c: &String| c.chars().take(7).collect::<String>();
    let mut commits = meta.commits.values();
    match (commits.next(), commits.count()) {
        (Some(first), 0) => short(first),
        (Some(first), more) => format!("{} +{more}", short(first)),
        (None, _) => "-".into(),
    }
}

pub fn ls(app: &App, json: bool) -> Result<()> {
    let rows: Vec<_> = kept(app)
        .into_iter()
        .map(|(profile, scope)| {
            let dir = root(&app.home).join(&profile).join(&scope);
            let meta = cache::read_meta(&dir);
            let status = status(app, &profile, &scope, &dir);
            (profile, scope, meta, status)
        })
        .collect();
    if json {
        let rows: Vec<_> = rows
            .iter()
            .map(|(profile, scope, meta, status)| {
                serde_json::json!({
                    "profile": profile,
                    "scope": scope,
                    "status": status,
                    "built_at": meta.as_ref().map(|m| m.built_at),
                    "commits": meta.as_ref().map(|m| &m.commits),
                    "base_tokens": meta.as_ref().and_then(|m| m.base_tokens),
                    "turns": meta.as_ref().map(|m| m.turns),
                    "duration_secs": meta.as_ref().map(|m| m.duration_secs),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no warm-ups kept; a profile with [warmup] builds one with `plete warmup refresh <profile> <repo or project>`");
        return Ok(());
    }
    println!(
        "{:<16} {:<28} {:<10} {:<10} {:<7} {:<6} {:<6} STATUS",
        "PROFILE", "SCOPE", "BUILT", "COMMIT", "SIZE", "TURNS", "TIME"
    );
    for (profile, scope, meta, status) in &rows {
        let (built, commit, size, turns, time) = match meta {
            Some(m) => (
                age_label(m.built_at),
                commit_label(m),
                tokens(m.base_tokens),
                m.turns.to_string(),
                format!("{}s", m.duration_secs),
            ),
            None => ("-".into(), "-".into(), "-".into(), "-".into(), "-".into()),
        };
        println!(
            "{profile:<16} {scope:<28} {built:<10} {commit:<10} {size:<7} {turns:<6} {time:<6} {status}"
        );
    }
    Ok(())
}

pub fn show(app: &App, profile: &str, scope: &str) -> Result<()> {
    let dir = cache::dir(&root(&app.home), profile, scope)?;
    match cache::read_context(&dir) {
        Some(text) => {
            print!("{text}");
            Ok(())
        }
        None => bail!("no warm-up of {profile} for {scope}; `plete warmup ls` lists them"),
    }
}

pub fn refresh(app: &App, profile: &str, scope: Option<&str>) -> Result<()> {
    let scopes = match scope {
        Some(scope) => vec![scope.to_string()],
        None => {
            let kept = subdirs(&root(&app.home).join(cache::safe_name(profile)?));
            if kept.is_empty() {
                bail!(
                    "no warm-up of {profile} is kept yet: name the repo (its store id) or \
                     project to build one for"
                );
            }
            kept
        }
    };
    let mut failed = Vec::new();
    for scope in &scopes {
        match refresh_one(app, profile, scope) {
            Ok(meta) => eprintln!(
                "plete: warmed up {profile} for {scope}: {} turns, {}s, {} tokens",
                meta.turns,
                meta.duration_secs,
                tokens(meta.base_tokens)
            ),
            Err(e) => {
                eprintln!("plete: {e:#}");
                failed.push(scope.as_str());
            }
        }
    }
    match failed.is_empty() {
        true => Ok(()),
        false => bail!("no warm-up built for {}", failed.join(", ")),
    }
}

fn refresh_one(app: &App, profile: &str, scope: &str) -> Result<Meta> {
    let (profile, cfg) = warmup_profile(app, profile, Some(scope))?;
    let source = build::source_for(app, cfg.per, scope)?;
    // The name a repo is kept under, whichever way it was named.
    let kept = source.scope();
    eprintln!("plete: warming up {} for {kept} …", profile.name);
    match build::ensure(app, &profile, &cfg, &source, true)? {
        Ensured::Built(meta) | Ensured::Fresh(meta) => Ok(meta),
    }
}

pub fn clear(app: &App, profile: Option<&str>, scope: Option<&str>) -> Result<()> {
    if scope.is_some() && profile.is_none() {
        bail!("name the profile as well as the scope");
    }
    let all = kept(app);
    let targets: Vec<_> = all
        .into_iter()
        .filter(|(p, s)| profile.is_none_or(|n| n == p) && scope.is_none_or(|n| n == s))
        .collect();
    if targets.is_empty() {
        eprintln!("plete: no warm-up to clear");
        return Ok(());
    }
    let spec = AgentSpec::from_config(&app.config).ok();
    for (profile, scope) in &targets {
        let dir = cache::dir(&root(&app.home), profile, scope)?;
        if lock::is_held(&lock::key_lock(&dir)) {
            eprintln!("plete: {profile}/{scope} is being built; waiting for it to finish");
        }
        let _held = lock::acquire(&lock::key_lock(&dir))?;
        remove(app, spec.as_ref(), &dir)?;
        eprintln!("plete: cleared {profile}/{scope}");
        // A profile with none left leaves no directory.
        let _ = std::fs::remove_dir(root(&app.home).join(profile));
    }
    Ok(())
}

/// Delete the warm-up in `dir`: its base session, its worktrees through git
/// (not `rm -rf` on a live one), then the rest.
fn remove(app: &App, spec: Option<&AgentSpec>, dir: &Path) -> Result<()> {
    let work = cache::work_dir(dir);
    if let (Some(spec), Some(meta)) = (spec, cache::read_meta(dir)) {
        spec.harness.forget_session(&work, &meta.base_session);
    }
    for name in subdirs_any(&work) {
        let wt = work.join(&name);
        let trunk = trunk_of(app, &name, &wt);
        let removed = trunk.is_some_and(|t| git::worktree_remove(&t, &wt, true).is_ok());
        if !removed {
            // Git does not know it: nothing live to protect.
            let _ = std::fs::remove_dir_all(&wt);
        }
    }
    std::fs::remove_dir_all(dir).with_context(|| format!("remove {}", dir.display()))
}

fn subdirs_any(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The repo a worktree under `work/` belongs to: the store's repo it is named
/// for, else the one git says it is linked to.
fn trunk_of(app: &App, name: &str, wt: &Path) -> Option<std::path::PathBuf> {
    let from_store = app
        .db
        .list_repos(None)
        .ok()?
        .into_iter()
        .find(|r| build::dir_name(&r.id) == name)
        .map(|r| std::path::PathBuf::from(r.trunk_path));
    from_store.or_else(|| {
        let common = git::git_common_dir(wt).ok()?;
        common.parent().map(Path::to_path_buf)
    })
}
