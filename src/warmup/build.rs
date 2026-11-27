//! Building a base: the read-only headless session, run in `work/`, and what
//! is kept of it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use super::cache::{self, Meta};
use super::config::{Config, Per};
use super::fresh;
use super::git::{Repos, Tip};
use super::lock;
use super::{argv, now_secs, root, utc_offset};
use crate::agent::AgentSpec;
use crate::context::App;
use crate::db::tasks::Role;
use crate::db::RepoRecord;
use crate::harness::Harness;
use crate::permissions::{self, Outcome};
use crate::profiles::Profile;
use crate::{git, projects, store, worktrees};

/// What a warm-up is of.
#[derive(Debug, Clone)]
pub enum Source {
    Repo(RepoRecord),
    Project {
        name: String,
        repos: Vec<RepoRecord>,
    },
}

impl Source {
    /// The name its warm-up is kept under.
    pub fn scope(&self) -> String {
        match self {
            Source::Repo(repo) => dir_name(&repo.id),
            Source::Project { name, .. } => name.clone(),
        }
    }

    fn repos(&self) -> &[RepoRecord] {
        match self {
            Source::Repo(repo) => std::slice::from_ref(repo),
            Source::Project { repos, .. } => repos,
        }
    }
}

/// A repo's id as one directory name, as the store's fetch locks flatten it.
pub fn dir_name(repo_id: &str) -> String {
    repo_id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => c,
            _ => '_',
        })
        .collect()
}

/// One repo of a warm-up, at the default branch as fetched.
#[derive(Debug, Clone)]
pub struct Repo {
    /// Its directory under `work/`.
    pub name: String,
    pub trunk: PathBuf,
    /// The default branch, e.g. `origin/main`.
    pub rev: String,
}

/// Everything one build needs.
pub struct Job<'a> {
    /// The `warmup` directory of the plete home.
    pub root: &'a Path,
    pub profile: &'a str,
    pub scope: &'a str,
    pub cfg: &'a Config,
    /// The agent's program alone: its configured arguments (which may allow
    /// anything) are not the warm-up's.
    pub program: &'a str,
    pub model: Option<&'a str>,
    pub harness: &'a dyn Harness,
    pub fingerprint: &'a str,
    pub repos: &'a [Repo],
    /// Where each build is logged.
    pub actions_log: &'a Path,
}

/// How a call to [`ensure`] ended.
#[derive(Debug)]
pub enum Ensured {
    /// There was a fresh base (a build that ran meanwhile counts).
    Fresh(Meta),
    Built(Meta),
}

/// A fresh base of `source` for `profile`: the one kept if it is fresh, else
/// a new one built now. `force` builds whatever is kept. Builds of one key
/// take turns, and a caller that waited on another's finds it done.
pub fn ensure(
    app: &App,
    profile: &Profile,
    cfg: &Config,
    source: &Source,
    force: bool,
) -> Result<Ensured> {
    let spec = AgentSpec::from_config(&app.config)?;
    let model = model_of(app, profile);
    let print = fingerprint(&spec, cfg, model);
    let repos = fetch(app, source)?;
    let scope = source.scope();
    let root = root(&app.home);
    let dir = cache::dir(&root, &profile.name, &scope)?;
    let log = permissions::actions_log(&app.home);
    let job = Job {
        root: &root,
        profile: &profile.name,
        scope: &scope,
        cfg,
        program: &spec.argv[0],
        model,
        harness: spec.harness,
        fingerprint: &print,
        repos: &repos,
        actions_log: &log,
    };
    let built = lock::serialised(
        &root,
        &dir,
        lock::MAX_BUILDS,
        || Ok(force || stale(&dir, cfg, &print, &repos).is_some()),
        || run(&job),
    )?;
    match built {
        Some(meta) => Ok(Ensured::Built(meta)),
        None => cache::read_meta(&dir)
            .map(Ensured::Fresh)
            .ok_or_else(|| anyhow!("the warm-up of {} for {scope} vanished", profile.name)),
    }
}

/// The model the profile's warm-up (and its tasks) run.
pub fn model_of<'a>(app: &'a App, profile: &'a Profile) -> Option<&'a str> {
    profile
        .settings
        .model
        .as_deref()
        .or(app.config.models.for_role(Role::Work))
}

/// What a base was built under, for `spec` (see [`fresh::fingerprint`]).
pub fn fingerprint(spec: &AgentSpec, cfg: &Config, model: Option<&str>) -> String {
    let version = crate::harness::program_version(&spec.argv[0]).unwrap_or_default();
    fresh::fingerprint(cfg, model, &version)
}

/// Why the base kept in `dir` is stale, `None` if it is fresh, by the repos'
/// default branches as last fetched.
pub fn stale(dir: &Path, cfg: &Config, print: &str, repos: &[Repo]) -> Option<fresh::Why> {
    let tips = Repos(
        repos
            .iter()
            .map(|r| {
                let tip = Tip {
                    dir: r.trunk.clone(),
                    rev: r.rev.clone(),
                };
                (r.name.clone(), tip)
            })
            .collect(),
    );
    let now = now_secs();
    fresh::staleness(
        cache::read_meta(dir).as_ref(),
        cfg,
        print,
        now,
        utc_offset(now),
        &tips,
    )
}

/// The default branch of each of `source`'s repos, freshly fetched.
fn fetch(app: &App, source: &Source) -> Result<Vec<Repo>> {
    let mut repos = Vec::new();
    for repo in source.repos() {
        repos.push(tip_of(repo, Some(app))?);
    }
    if repos.is_empty() {
        bail!("{} has no repositories to warm up", source.scope());
    }
    Ok(repos)
}

/// `repo`'s default branch: fetched first under the store's lock when `app`
/// is given, else as last fetched.
pub fn tip_of(repo: &RepoRecord, fetched: Option<&App>) -> Result<Repo> {
    let trunk = PathBuf::from(&repo.trunk_path);
    if !trunk.is_dir() {
        bail!("{}: trunk is missing: {}", repo.id, trunk.display());
    }
    if let Some(app) = fetched {
        let _lock = store::lock_repo(&app.home, &repo.id)?;
        git::fetch_prune(&trunk).with_context(|| format!("fetch {}", repo.id))?;
    }
    let rev = worktrees::resolve_start_point(&trunk, &repo.default_branch)
        .with_context(|| repo.id.clone())?;
    Ok(Repo {
        name: dir_name(&repo.id),
        trunk,
        rev,
    })
}

/// The repos of `source` as last fetched, for a status that fetches nothing.
pub fn tips(source: &Source) -> Vec<Repo> {
    source
        .repos()
        .iter()
        .filter_map(|r| tip_of(r, None).ok())
        .collect()
}

/// A project's repositories, each spec found in the store.
pub fn project_source(app: &App, name: &str) -> Result<Source> {
    let project = projects::load(app, name)?;
    let repos = project
        .config
        .repos
        .iter()
        .map(|spec| crate::resolve::resolve_repo_filter(&app.db, spec))
        .collect::<Result<Vec<_>>>()
        .with_context(|| format!("project {name}'s repos"))?;
    Ok(Source::Project {
        name: name.to_string(),
        repos,
    })
}

/// What `scope` names for a profile keyed `per`: a repo, by the name its
/// warm-up is kept under or as `plete worktree add` finds one, or a project.
pub fn source_for(app: &App, per: Per, scope: &str) -> Result<Source> {
    match per {
        Per::Project => project_source(app, scope),
        Per::Repo => {
            let repos = app.db.list_repos(None)?;
            match repos.into_iter().find(|r| dir_name(&r.id) == scope) {
                Some(repo) => Ok(Source::Repo(repo)),
                None => Ok(Source::Repo(crate::resolve::resolve_repo_filter(
                    &app.db, scope,
                )?)),
            }
        }
    }
}

/// What the headless run printed that matters: the final message and its turns.
#[derive(Debug, PartialEq, Eq)]
struct Reply {
    result: String,
    turns: u32,
}

/// Claude Code's `--output-format json`. Out of turns is a failure, not a
/// short summary: it has no `result`.
fn parse_reply(text: &str, max_turns: u32) -> Result<Reply> {
    let v: serde_json::Value =
        serde_json::from_str(text.trim()).context("the warm-up printed no JSON")?;
    let subtype = v["subtype"].as_str().unwrap_or_default();
    if subtype == "error_max_turns" {
        bail!("it used all {max_turns} turns without finishing (`max_turns`)");
    }
    if v["is_error"] == true || subtype.starts_with("error") {
        let why = v["result"].as_str().filter(|r| !r.is_empty());
        bail!("it failed: {}", why.unwrap_or(subtype));
    }
    let result = v["result"].as_str().unwrap_or_default().trim();
    if result.is_empty() {
        bail!("it ended without a summary");
    }
    Ok(Reply {
        result: result.to_string(),
        turns: v["num_turns"]
            .as_u64()
            .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX)),
    })
}

/// Put the warm-up's worktree of `repo` at `commit`: reuse the one there,
/// else make it.
fn checkout(work: &Path, repo: &Repo, commit: &str) -> Result<()> {
    let wt = work.join(&repo.name);
    if wt.exists() {
        if git::checkout_detached(&wt, commit).is_ok() {
            return Ok(());
        }
        // Not one git knows, or not clean: start it over, through git.
        let _ = git::worktree_remove(&repo.trunk, &wt, true);
        let _ = std::fs::remove_dir_all(&wt);
    }
    let _ = git::worktree_prune(&repo.trunk);
    git::worktree_add_detached(&repo.trunk, &wt, commit)
        .with_context(|| format!("check out {} for the warm-up", repo.name))
}

/// Build a base now and keep it, replacing the one before. Logs the build to
/// `actions.log`. On failure the base kept is left as it was.
pub fn run(job: &Job) -> Result<Meta> {
    let started = Instant::now();
    let built = build(job);
    let secs = started.elapsed().as_secs();
    let what = format!("warm-up {}/{}", job.profile, job.scope);
    let (action, logged, error) = match &built {
        Ok(meta) => (
            format!("built {what}: {} turns, {secs}s", meta.turns),
            Outcome::Done,
            None,
        ),
        Err(e) => (
            format!("built {what}: {secs}s"),
            Outcome::Failed,
            Some(format!("{e:#}")),
        ),
    };
    if let Err(e) =
        permissions::append(job.actions_log, "warmup", &action, logged, error.as_deref())
    {
        eprintln!(
            "plete: warning: could not write {}: {e:#}",
            job.actions_log.display()
        );
    }
    built.with_context(|| format!("warm-up of {} for {} failed", job.profile, job.scope))
}

fn build(job: &Job) -> Result<Meta> {
    let dir = cache::dir(job.root, job.profile, job.scope)?;
    let work = cache::work_dir(&dir);
    std::fs::create_dir_all(&work).with_context(|| format!("create {}", work.display()))?;
    let mut commits = BTreeMap::new();
    for repo in job.repos {
        let commit = git::rev_parse(&repo.trunk, &repo.rev)
            .ok_or_else(|| anyhow!("{} has no {}", repo.name, repo.rev))?;
        checkout(&work, repo, &commit)?;
        commits.insert(repo.name.clone(), commit);
    }

    let base = uuid::Uuid::new_v4().to_string();
    let Some(mut args) = argv::argv(job.cfg, job.harness, &base) else {
        bail!("{} cannot run a warm-up headless", job.harness.name());
    };
    if let Some(model) = job.model {
        args.extend(["--model".to_string(), model.to_string()]);
    }
    let started = Instant::now();
    let out = run_headless(job, &work, &args, &dir);
    let duration_secs = started.elapsed().as_secs();
    let reply = out.and_then(|text| parse_reply(&text, job.cfg.max_turns));
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => {
            // Whatever the failed run left of its session.
            job.harness.forget_session(&work, &base);
            return Err(e);
        }
    };

    let info = job.harness.transcript_info(&work, &base);
    let meta = Meta {
        base_session: base.clone(),
        built_at: now_secs(),
        commits,
        fingerprint: job.fingerprint.to_string(),
        files_read: info
            .as_ref()
            .map(|i| i.files_read.clone())
            .unwrap_or_default(),
        turns: reply.turns,
        duration_secs,
        base_tokens: info.map(|i| i.last_input_tokens),
    };
    let before = cache::read_meta(&dir);
    cache::write_context(&dir, &format!("{}\n", reply.result))?;
    cache::write_meta(&dir, &meta)?;
    // Only the latest base is kept.
    if let Some(old) = before.filter(|m| m.base_session != base) {
        job.harness.forget_session(&work, &old.base_session);
    }
    Ok(meta)
}

/// Run the warm-up with `args` in `work` and return what it printed, killing
/// its whole process group at the profile's `timeout`.
fn run_headless(job: &Job, work: &Path, args: &[String], dir: &Path) -> Result<String> {
    let out_path = dir.join("build.out");
    let err_path = dir.join("build.err");
    let out = std::fs::File::create(&out_path)
        .with_context(|| format!("create {}", out_path.display()))?;
    let err = std::fs::File::create(&err_path)
        .with_context(|| format!("create {}", err_path.display()))?;
    let mut cmd = Command::new(job.program);
    cmd.args(args)
        .current_dir(work)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    // Nothing of a task's: it is no task, and has none of plete's hooks.
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if name.starts_with("PLETE_") || name.starts_with("JEET_") {
            cmd.env_remove(&*name);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("start `{}`", job.program))?;
    let limit = Duration::from_secs(job.cfg.timeout.secs());
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().context("wait for the warm-up")? {
            break status;
        }
        if started.elapsed() >= limit {
            kill_group(&mut child);
            bail!("it timed out after {}s (`timeout`)", limit.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let text = std::fs::read_to_string(&out_path).unwrap_or_default();
    if !status.success() {
        // `--max-turns` exhausted exits 1 with the reason in its JSON.
        if text.trim_start().starts_with('{') {
            if let Err(reason) = parse_reply(&text, job.cfg.max_turns) {
                bail!("{reason:#}");
            }
        }
        let said = std::fs::read_to_string(&err_path).unwrap_or_default();
        let said = said.lines().rev().find(|l| !l.trim().is_empty());
        bail!(
            "`{}` exited with {status}: {}",
            job.program,
            said.unwrap_or("no output")
        );
    }
    Ok(text)
}

/// Kill `child` and everything it started, which share its process group.
fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // SAFETY: a signal to the group the child was made leader of.
        unsafe {
            libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::claude::ClaudeCode;
    use crate::harness::Transcript;
    use crate::warmup::config::Span;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    /// Claude Code's flags, with a transcript and a session store of its own.
    struct Fake {
        forgotten: Mutex<Vec<String>>,
    }

    impl Harness for Fake {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn name(&self) -> &'static str {
            "Fake"
        }
        fn default_command(&self) -> Option<&'static str> {
            None
        }
        fn headless_args(
            &self,
            prompt: &str,
            opts: &crate::harness::Headless,
        ) -> Option<Vec<String>> {
            ClaudeCode.headless_args(prompt, opts)
        }
        fn transcript_info(&self, _dir: &Path, _id: &str) -> Option<Transcript> {
            Some(Transcript {
                last_input_tokens: 1234,
                files_read: vec!["r/src/a.rs".into()],
            })
        }
        fn forget_session(&self, _dir: &Path, id: &str) {
            self.forgotten.lock().unwrap().push(id.to_string());
        }
    }

    const OK: &str = r##"{"type":"result","subtype":"success","is_error":false,"result":"# the map","num_turns":4}"##;

    struct Lab {
        tmp: tempfile::TempDir,
        harness: Fake,
        cfg: Config,
    }

    impl Lab {
        fn new() -> Lab {
            let lab = Lab {
                tmp: tempfile::tempdir().unwrap(),
                harness: Fake {
                    forgotten: Mutex::new(Vec::new()),
                },
                cfg: toml::from_str("prompt = \"map it\"\nper = \"repo\"\n").unwrap(),
            };
            let trunk = lab.tmp.path().join("trunk");
            std::fs::create_dir(&trunk).unwrap();
            let commit = [
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "c",
            ];
            for args in [&["init", "-q", "-b", "main"][..], &commit[..]] {
                let ok = Command::new("git")
                    .arg("-C")
                    .arg(&trunk)
                    .args(args)
                    .status()
                    .unwrap();
                assert!(ok.success());
            }
            lab
        }

        fn root(&self) -> PathBuf {
            self.tmp.path().join("home/warmup")
        }

        fn dir(&self) -> PathBuf {
            self.root().join("p/r")
        }

        fn log(&self) -> PathBuf {
            self.tmp.path().join("actions.log")
        }

        /// A fake `claude` that records how it was run, then runs `body`.
        fn script(&self, body: &str) -> PathBuf {
            let path = self.tmp.path().join("claude");
            let text = format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{0}/args'\npwd > '{0}/cwd'\nenv > '{0}/env'\n{body}\n",
                self.tmp.path().display()
            );
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        fn build(&self, program: &Path) -> Result<Meta> {
            let repos = [Repo {
                name: "r".into(),
                trunk: self.tmp.path().join("trunk"),
                rev: "main".into(),
            }];
            let root = self.root();
            let log = self.log();
            let program = program.to_string_lossy();
            run(&Job {
                root: &root,
                profile: "p",
                scope: "r",
                cfg: &self.cfg,
                program: &program,
                model: Some("sonnet"),
                harness: &self.harness,
                fingerprint: "fp",
                repos: &repos,
                actions_log: &log,
            })
        }

        fn forgotten(&self) -> Vec<String> {
            self.harness.forgotten.lock().unwrap().clone()
        }
    }

    #[test]
    fn a_build_keeps_meta_and_context_and_logs_turns_and_time() {
        let lab = Lab::new();
        let meta = lab.build(&lab.script(&format!("echo '{OK}'"))).unwrap();
        let dir = lab.dir();
        assert_eq!(meta.turns, 4);
        assert_eq!(meta.base_tokens, Some(1234));
        assert_eq!(meta.files_read, ["r/src/a.rs"]);
        assert_eq!(meta.fingerprint, "fp");
        assert_eq!(cache::read_meta(&dir), Some(meta.clone()));
        assert_eq!(cache::read_context(&dir).as_deref(), Some("# the map\n"));
        let commit = git::rev_parse(&lab.tmp.path().join("trunk"), "main").unwrap();
        assert_eq!(meta.commits["r"], commit);
        // Its cwd is `work/`, and the worktree is at the commit.
        let cwd = std::fs::read_to_string(lab.tmp.path().join("cwd")).unwrap();
        assert_eq!(
            Path::new(cwd.trim()).canonicalize().unwrap(),
            cache::work_dir(&dir).canonicalize().unwrap()
        );
        let wt = cache::work_dir(&dir).join("r");
        assert_eq!(git::rev_parse(&wt, "HEAD").unwrap(), commit);
        // Run as the argv says: read-only, its own session id, the model.
        let args = std::fs::read_to_string(lab.tmp.path().join("args")).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert!(args.contains(&"dontAsk") && args.contains(&"--strict-mcp-config"));
        assert!(args
            .windows(2)
            .any(|w| w == ["--session-id", meta.base_session.as_str()]));
        assert!(args.windows(2).any(|w| w == ["--model", "sonnet"]));
        // The log has the profile, scope, turns, time and outcome.
        let log = std::fs::read_to_string(lab.log()).unwrap();
        assert!(log.contains("warm-up p/r: 4 turns"), "{log}");
        assert!(log.contains("\"outcome\":\"done\""), "{log}");
    }

    #[test]
    fn the_warmup_gets_no_task_environment() {
        let lab = Lab::new();
        std::env::set_var("PLETE_WARMUP_TEST_TASK", "t");
        std::env::set_var("JEET_WARMUP_TEST_TASK", "t");
        lab.build(&lab.script(&format!("echo '{OK}'"))).unwrap();
        let env = std::fs::read_to_string(lab.tmp.path().join("env")).unwrap();
        assert!(!env.contains("PLETE_") && !env.contains("JEET_"), "{env}");
    }

    #[test]
    fn a_newer_base_replaces_the_older_which_is_forgotten() {
        let lab = Lab::new();
        let script = lab.script(&format!("echo '{OK}'"));
        let first = lab.build(&script).unwrap();
        assert!(lab.forgotten().is_empty());
        let second = lab.build(&script).unwrap();
        assert_ne!(first.base_session, second.base_session);
        assert_eq!(lab.forgotten(), [first.base_session]);
        let kept = cache::read_meta(&lab.dir()).unwrap();
        assert_eq!(kept.base_session, second.base_session);
    }

    #[test]
    fn a_failed_build_leaves_the_old_base() {
        let lab = Lab::new();
        let first = lab.build(&lab.script(&format!("echo '{OK}'"))).unwrap();
        let fails = lab.script("echo 'boom' >&2; exit 3");
        let err = format!("{:#}", lab.build(&fails).unwrap_err());
        assert!(err.contains("boom"), "{err}");
        assert_eq!(cache::read_meta(&lab.dir()), Some(first.clone()));
        let context = cache::read_context(&lab.dir());
        assert_eq!(context.as_deref(), Some("# the map\n"));
        // Only what the failed run started is forgotten.
        let forgotten = lab.forgotten();
        assert_eq!(forgotten.len(), 1);
        assert_ne!(forgotten[0], first.base_session);
        let log = std::fs::read_to_string(lab.log()).unwrap();
        assert!(
            log.contains("\"outcome\":\"failed\"") && log.contains("boom"),
            "{log}"
        );
    }

    #[test]
    fn running_out_of_turns_is_a_failure() {
        let lab = Lab::new();
        let out = r#"{"type":"result","subtype":"error_max_turns","is_error":true,"num_turns":31}"#;
        let err = lab
            .build(&lab.script(&format!("echo '{out}'; exit 1")))
            .unwrap_err();
        assert!(format!("{err:#}").contains("30 turns"), "{err:#}");
        assert_eq!(cache::read_meta(&lab.dir()), None);
        // Even a zero exit with an error result is one.
        let err = lab
            .build(&lab.script(&format!("echo '{out}'")))
            .unwrap_err();
        assert!(format!("{err:#}").contains("30 turns"), "{err:#}");
    }

    #[test]
    fn a_timeout_kills_the_whole_group_and_fails() {
        let mut lab = Lab::new();
        lab.cfg.timeout = Span(3);
        let started = Instant::now();
        // A child of its own, which must not outlive it.
        let slow = lab.script(&format!(
            "sleep 30 &\necho $! > '{}/child'\nsleep 30",
            lab.tmp.path().display()
        ));
        let err = lab.build(&slow).unwrap_err();
        assert!(format!("{err:#}").contains("timed out after 3s"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(cache::read_meta(&lab.dir()), None);
        let child = std::fs::read_to_string(lab.tmp.path().join("child")).unwrap();
        let pid: u32 = child.trim().parse().unwrap();
        // Gone, or a zombie not yet reaped by init.
        let mut gone = false;
        for _ in 0..50 {
            let state = Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&state.stdout);
            if state.trim().is_empty() || state.trim().starts_with('Z') {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(gone, "the child outlived the warm-up");
    }

    #[test]
    fn a_reply_needs_a_result() {
        assert_eq!(
            parse_reply(OK, 30).unwrap(),
            Reply {
                result: "# the map".into(),
                turns: 4
            }
        );
        for bad in [
            "",
            "not json",
            r#"{"subtype":"success","result":""}"#,
            r#"{"subtype":"success"}"#,
            r#"{"subtype":"error_during_execution","is_error":true,"result":"x"}"#,
            r#"{"subtype":"success","is_error":true,"result":"x"}"#,
        ] {
            assert!(parse_reply(bad, 30).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_repo_id_is_one_directory_name() {
        assert_eq!(
            dir_name("github.com/peterddod/jeet"),
            "github.com_peterddod_jeet"
        );
        assert!(cache::safe_name(&dir_name("host/o/r")).is_ok());
    }
}
