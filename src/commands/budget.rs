//! `plete budget`: what plete itself runs for budgets, hidden.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::cli::BudgetCommands;
use crate::context::App;
use crate::manager::classifier;
use crate::tasks;

pub fn run(app: &App, command: BudgetCommands) -> Result<()> {
    match command {
        BudgetCommands::Classify { task, level } => {
            println!("{}", classify(app, &task, level.parse()?)?);
            Ok(())
        }
    }
}

/// Build the bundle for `task` crossing `level`%, ask the classifier and
/// return its answer as JSON. Applies nothing: the result of a dry run.
fn classify(app: &App, key: &str, level: u8) -> Result<serde_json::Value> {
    let task = tasks::find(app, key)?;
    let policy = app.config.budget_policy()?;
    let row = app.db.budget(&task.id)?.unwrap_or_default();
    let bounds = classifier::Bounds::of(&row);
    let bundle = classifier::build_bundle(app, &task, level, &bounds)?;
    let prompt = classifier::prompt(app, &bundle)?;
    let (invocation, _dir) = classifier::build_invocation(app, &policy)?;
    let started = Instant::now();
    let answer = classifier::run(
        &invocation,
        &prompt,
        Duration::from_secs(policy.classifier_timeout_secs),
    )
    .with_context(|| format!("the classifier ({})", policy.classifier_model))?;
    let decision = classifier::parse_decision(&answer)?;
    let grow = match decision.by {
        Some(by) => Some(budget_label(classifier::clamp_grow(by, &bounds)?)),
        None => None,
    };
    Ok(serde_json::json!({
        "level": level,
        "decision": decision.kind.as_str(),
        "by": decision.by.map(budget_label),
        "clamped": grow,
        "reason": decision.reason,
        "model": policy.classifier_model,
        "input_bytes": prompt.len(),
        "duration_ms": started.elapsed().as_millis() as u64,
    }))
}

/// `+1h,20turns`.
fn budget_label(limits: crate::manager::budget::Limits) -> String {
    format!("+{}", crate::manager::budget::label(limits))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::db::tasks::{RunState, TaskKind, TaskRecord, TaskState};
    use crate::db::Database;
    use std::os::unix::fs::PermissionsExt;

    fn lab(script: &str) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let mut app = App {
            config: config::load_or_create(&home).unwrap(),
            db: Database::open(&config::index_path(&home)).unwrap(),
            new_home: None,
            home: home.clone(),
        };
        let root = crate::manager::root(&app).unwrap();
        app.db
            .insert_task(&TaskRecord {
                id: "m".into(),
                slug: "m".into(),
                name: "m".into(),
                project: None,
                kind: TaskKind::Work,
                state: TaskState::Active,
                created_at: 0,
                last_activity: 0,
                parent_id: Some(root.id),
                placement: crate::placement::for_new(),
                run_state: RunState::Run,
                run_state_by: None,
                max_children: None,
            })
            .unwrap();
        app.db.set_budget_max("m", Some(3600), None).unwrap();
        // A fake command: the script reads the prompt and prints `script`.
        let path = home.join("fake.sh");
        std::fs::write(&path, format!("#!/bin/sh\ncat > /dev/null\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        app.config.budget.classifier_command = Some(path.to_string_lossy().to_string());
        (dir, app)
    }

    #[test]
    fn classify_runs_the_pipeline_with_a_fake_command_and_applies_nothing() {
        let (_dir, app) = lab(r#"echo '{"decision":"grow","by":"+5m","reason":"progress"}'"#);
        let said = classify(&app, "m", 80).unwrap();
        assert_eq!(said["decision"], "grow");
        assert_eq!(said["by"], "+5m");
        // Raised to the minimum step.
        assert_eq!(said["clamped"], "+10m");
        assert_eq!(said["reason"], "progress");
        assert_eq!(said["level"], 80);
        assert_eq!(said["model"], "haiku");
        assert!(said["input_bytes"].as_u64().unwrap() > 100);
        // Nothing was applied.
        let row = app.db.budget("m").unwrap().unwrap();
        assert_eq!(
            (row.max_secs, row.grows, row.last_decision),
            (Some(3600), 0, None)
        );
    }

    #[test]
    fn classify_fails_on_an_answer_that_is_not_a_decision() {
        for script in ["echo sure thing", "exit 2", "true"] {
            let (_dir, app) = lab(script);
            assert!(classify(&app, "m", 100).is_err(), "{script}");
        }
        let (_dir, app) = lab(r#"echo '{"decision":"stop","reason":"loop"}'"#);
        assert!(classify(&app, "nope", 80).is_err());
        let said = classify(&app, "m", 100).unwrap();
        assert_eq!(
            (said["decision"].as_str(), said["clamped"].is_null()),
            (Some("stop"), true)
        );
    }
}
