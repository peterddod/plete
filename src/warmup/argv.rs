//! The warm-up session's command line: read-only by permissions, not by the
//! prompt. One pure function, so a test can pin every flag.
//!
//! The flags are Claude Code's (the only harness that runs headless today),
//! checked on 2.1.327 against a mock API:
//! - `--permission-mode dontAsk` denies every tool not allowed, in `-p`;
//! - `--setting-sources ""` stops the user's, project's and local settings
//!   (allow-lists, hooks, `env`) from widening it;
//! - the deny rules `Bash(*--output*)`, `Bash(*--pre*)` and
//!   `Bash(*--hostname-bin*)` stop what the allowed `git log|diff|show
//!   --output=FILE`, `rg --pre CMD` and `rg --hostname-bin CMD` could do:
//!   write a file, run a command. Other spellings of these rules
//!   (`Bash(git log:*--output*)`, `Bash(rg * --pre*)`) match nothing.

use super::config::Config;
use crate::harness::{Harness, Headless};

/// What the warm-up may use without asking: reading, and Bash that only reads.
pub const ALLOW: &[&str] = &[
    "Read",
    "Grep",
    "Glob",
    "Bash(git log:*)",
    "Bash(git show:*)",
    "Bash(git diff:*)",
    "Bash(git status:*)",
    "Bash(git ls-files:*)",
    "Bash(ls:*)",
    "Bash(cat:*)",
    "Bash(wc:*)",
    "Bash(rg:*)",
    "Bash(find:*)",
];

/// What it may not use whatever else allows it.
pub const DENY: &[&str] = &[
    "Edit",
    "Write",
    "NotebookEdit",
    "WebFetch",
    "WebSearch",
    "Bash(*--output*)",
    "Bash(*--pre*)",
    "Bash(*--hostname-bin*)",
];

/// Added to the profile's prompt.
const SUFFIX: &str = "\n\nWhen you have explored enough, end with a dense summary of what a \
colleague starting work here needs to know, under about 1,500 words. Use repo-relative paths \
only, and do not paste files. Your final message is saved as CONTEXT.md; you have no tool to \
write it and need none.";

/// The prompt: the profile's, then the fixed suffix.
pub fn prompt(cfg: &Config) -> String {
    format!("{}{SUFFIX}", cfg.prompt.trim_end())
}

/// The arguments (after the program) of the warm-up run that becomes session
/// `base_id`, or `None` when `harness` cannot run headless. Nothing of the
/// profile but its `[warmup]` section goes in: no MCP servers, `env`,
/// `[permissions]`, hooks or plete settings.
pub fn argv(cfg: &Config, harness: &dyn Harness, base_id: &str) -> Option<Vec<String>> {
    let to_owned = |tools: &[&str]| tools.iter().map(|t| t.to_string()).collect::<Vec<_>>();
    let (allow, deny) = (to_owned(ALLOW), to_owned(DENY));
    let opts = Headless {
        max_turns: cfg.max_turns,
        allow: &allow,
        deny: &deny,
    };
    let mut args = harness.headless_args(&prompt(cfg), &opts)?;
    args.extend(
        [
            "--session-id",
            base_id,
            "--output-format",
            "json",
            // Only the servers named by `--mcp-config`, and none is.
            "--strict-mcp-config",
            "--permission-mode",
            "dontAsk",
            "--setting-sources",
            "",
        ]
        .map(String::from),
    );
    Some(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::claude::ClaudeCode;

    const ID: &str = "6f1c1c3e-5b0a-4c53-9a53-2f0f8f3a9d11";

    fn config(extra: &str) -> Config {
        let text = format!("prompt = \"map it\"\nper = \"repo\"\n{extra}");
        toml::from_str(&text).unwrap()
    }

    fn args(cfg: &Config) -> Vec<String> {
        argv(cfg, &ClaudeCode, ID).unwrap()
    }

    fn value<'a>(args: &'a [String], flag: &str) -> &'a str {
        let at = args.iter().position(|a| a == flag).unwrap();
        &args[at + 1]
    }

    #[test]
    fn it_runs_headless_with_the_base_id_and_json_output() {
        let a = args(&config(""));
        assert_eq!(a[0], "-p");
        assert!(a[1].starts_with("map it") && a[1].contains("CONTEXT.md"));
        assert_eq!(value(&a, "--session-id"), ID);
        assert_eq!(value(&a, "--output-format"), "json");
        assert_eq!(value(&a, "--max-turns"), "30");
        assert_eq!(value(&args(&config("max_turns = 7")), "--max-turns"), "7");
    }

    #[test]
    fn it_is_read_only_by_permission_mode_not_by_the_prompt() {
        let a = args(&config(""));
        assert_eq!(value(&a, "--permission-mode"), "dontAsk");
        for bad in ["bypassPermissions", "acceptEdits", "auto", "plan"] {
            assert!(!a.iter().any(|x| x == bad), "{bad}");
        }
        assert!(!a.iter().any(|x| x.contains("dangerously")));
        // User, project and local settings do not widen it.
        let at = a.iter().position(|x| x == "--setting-sources").unwrap();
        assert_eq!(a[at + 1], "");
    }

    #[test]
    fn it_allows_only_reads_and_the_listed_bash() {
        let a = args(&config(""));
        let allow: Vec<&str> = value(&a, "--allowedTools").split(',').collect();
        assert_eq!(allow, ALLOW);
        assert_eq!(
            allow,
            [
                "Read",
                "Grep",
                "Glob",
                "Bash(git log:*)",
                "Bash(git show:*)",
                "Bash(git diff:*)",
                "Bash(git status:*)",
                "Bash(git ls-files:*)",
                "Bash(ls:*)",
                "Bash(cat:*)",
                "Bash(wc:*)",
                "Bash(rg:*)",
                "Bash(find:*)",
            ]
        );
        assert!(!allow
            .iter()
            .any(|t| matches!(*t, "Bash" | "Edit" | "Write")));
    }

    #[test]
    fn it_denies_writes_the_web_and_the_two_flags_that_make_reads_write() {
        let a = args(&config(""));
        let deny: Vec<&str> = value(&a, "--disallowedTools").split(',').collect();
        for tool in [
            "Edit",
            "Write",
            "NotebookEdit",
            "WebFetch",
            "WebSearch",
            // The only spellings of these rules that deny: the others
            // (`Bash(git log:*--output*)`, `Bash(rg * --pre*)`) match nothing.
            "Bash(*--output*)",
            "Bash(*--pre*)",
            "Bash(*--hostname-bin*)",
        ] {
            assert!(deny.contains(&tool), "{tool}");
        }
        assert_eq!(deny.len(), 8);
    }

    #[test]
    fn it_gives_no_mcp_hooks_settings_or_profile_permissions() {
        let a = args(&config(""));
        assert!(a.contains(&"--strict-mcp-config".to_string()));
        for flag in [
            "--mcp-config",
            "--settings",
            "--allow",
            "--add-dir",
            "--resume",
            "--fork-session",
            "--plugin-dir",
            "--append-system-prompt",
        ] {
            assert!(!a.iter().any(|x| x == flag), "{flag}");
        }
        assert!(!a.iter().any(|x| x.contains("plete") && x != &a[1]));
    }

    #[test]
    fn the_argv_depends_on_the_warmup_section_alone() {
        // `argv` takes the section, not the profile: a profile whose
        // `[permissions]` allow `Write`, or that has `env` and `mcp`, has
        // nothing to hand it.
        let profile = "+++\nenv = { A = \"1\" }\n[permissions]\nallow = [\"Write\"]\n\n\
                       [warmup]\nprompt = \"map it\"\nper = \"repo\"\n+++\n";
        let (settings, _) = crate::profiles::parse(profile).unwrap();
        assert!(!settings.permissions.is_empty());
        let a = args(&settings.warmup.unwrap());
        assert_eq!(a, args(&config("")));
        assert!(!value(&a, "--allowedTools").contains("Write"));
    }

    #[test]
    fn a_harness_that_cannot_run_headless_has_no_argv() {
        assert!(argv(&config(""), &crate::harness::Generic, ID).is_none());
    }
}
