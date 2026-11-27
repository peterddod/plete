//! The git questions freshness asks, answered by the `git` binary.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::fresh::GitView;
use crate::git::git_output;

/// Where one repo of a warm-up is and what its default branch is.
pub struct Tip {
    /// A checkout (the warm-up's worktree) or the repo itself.
    pub dir: PathBuf,
    /// The freshly fetched default branch, e.g. `origin/main`.
    pub rev: String,
}

/// A [`GitView`] over repos, keyed as `Meta::commits` is.
pub struct Repos(pub BTreeMap<String, Tip>);

impl Repos {
    fn run(
        &self,
        repo: &str,
        mut args: Vec<String>,
        base: &str,
        paths: &[String],
    ) -> Option<String> {
        let tip = self.0.get(repo)?;
        args.push(format!("{base}..{}", tip.rev));
        args.push("--".into());
        args.extend(paths.iter().cloned());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        git_output(&tip.dir, &args, &[], None).ok()
    }
}

impl GitView for Repos {
    fn commits_since(&self, repo: &str, base: &str, paths: &[String]) -> Option<u64> {
        let out = self.run(repo, vec!["rev-list".into(), "--count".into()], base, paths)?;
        out.trim().parse().ok()
    }

    fn lines_since(&self, repo: &str, base: &str, paths: &[String]) -> Option<u64> {
        let out = self.run(repo, vec!["diff".into(), "--shortstat".into()], base, paths)?;
        Some(parse_shortstat(&out))
    }

    fn changed_files(&self, repo: &str, base: &str) -> Option<Vec<String>> {
        let out = self.run(
            repo,
            vec!["diff".into(), "--name-only".into(), "-z".into()],
            base,
            &[],
        )?;
        Some(
            out.split('\0')
                .filter(|f| !f.is_empty())
                .map(str::to_string)
                .collect(),
        )
    }
}

/// Insertions plus deletions in `git diff --shortstat`'s one line, such as
/// ` 3 files changed, 10 insertions(+), 2 deletions(-)`; an empty diff is 0.
fn parse_shortstat(out: &str) -> u64 {
    out.split(',')
        .filter_map(|part| {
            let part = part.trim();
            let n = part.split_whitespace().next()?.parse::<u64>().ok()?;
            (part.contains("insertion") || part.contains("deletion")).then_some(n)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .status()
            .unwrap();
        assert!(ok.success(), "git {args:?}");
    }

    fn commit(dir: &Path, file: &str, text: &str) {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-qm", "c"]);
    }

    fn head(dir: &Path) -> String {
        crate::git::rev_parse(dir, "HEAD").unwrap()
    }

    #[test]
    fn shortstat_sums_insertions_and_deletions() {
        assert_eq!(parse_shortstat(""), 0);
        assert_eq!(parse_shortstat(" 1 file changed, 3 insertions(+)\n"), 3);
        assert_eq!(parse_shortstat(" 1 file changed, 2 deletions(-)\n"), 2);
        assert_eq!(
            parse_shortstat(" 3 files changed, 10 insertions(+), 2 deletions(-)\n"),
            12
        );
    }

    #[test]
    fn counts_come_from_git_with_and_without_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        commit(dir, "src/a.rs", "one\n");
        let base = head(dir);
        commit(dir, "src/a.rs", "one\ntwo\nthree\n");
        commit(dir, "docs/d.md", "x\ny\n");
        let repos = Repos(BTreeMap::from([(
            "r".to_string(),
            Tip {
                dir: dir.to_path_buf(),
                rev: "main".into(),
            },
        )]));

        assert_eq!(repos.commits_since("r", &base, &[]), Some(2));
        assert_eq!(repos.commits_since("r", &base, &["src/".into()]), Some(1));
        assert_eq!(
            repos.commits_since("r", &base, &["nothing/".into()]),
            Some(0)
        );
        assert_eq!(repos.lines_since("r", &base, &[]), Some(4));
        assert_eq!(repos.lines_since("r", &base, &["src/".into()]), Some(2));
        assert_eq!(repos.lines_since("r", &base, &["nothing/".into()]), Some(0));
        let mut files = repos.changed_files("r", &base).unwrap();
        files.sort();
        assert_eq!(files, ["docs/d.md", "src/a.rs"]);

        // Nothing since the tip; an unknown base or repo is no answer.
        let tip = head(dir);
        assert_eq!(repos.commits_since("r", &tip, &[]), Some(0));
        assert_eq!(repos.lines_since("r", &tip, &[]), Some(0));
        assert_eq!(repos.commits_since("r", "deadbeef", &[]), None);
        assert_eq!(repos.commits_since("other", &base, &[]), None);
    }
}
