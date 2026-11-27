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
    /// `git <args> <base>..<tip> -- <paths>` in `repo`'s dir. `base` comes from
    /// a file, so anything that is not a hex object name is no answer rather
    /// than an option for git. `paths` are plain repo-relative prefixes, not
    /// pathspec magic.
    fn run(&self, repo: &str, args: &[&str], base: &str, paths: &[String]) -> Option<String> {
        if base.is_empty() || !base.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let tip = self.0.get(repo)?;
        let range = format!("{base}..{}", tip.rev);
        let mut argv = vec!["--literal-pathspecs"];
        argv.extend_from_slice(args);
        argv.push(&range);
        argv.push("--");
        argv.extend(paths.iter().map(String::as_str));
        git_output(&tip.dir, &argv, &[], None).ok()
    }
}

impl GitView for Repos {
    fn commits_since(&self, repo: &str, base: &str, paths: &[String]) -> Option<u64> {
        let out = self.run(repo, &["rev-list", "--count"], base, paths)?;
        out.trim().parse().ok()
    }

    fn lines_since(&self, repo: &str, base: &str, paths: &[String]) -> Option<u64> {
        let out = self.run(repo, &["diff", "--numstat", "--no-renames"], base, paths)?;
        Some(parse_numstat(&out))
    }

    fn changed_files(&self, repo: &str, base: &str) -> Option<Vec<String>> {
        // No rename detection, so a moved file shows as its old path deleted
        // and its new one added.
        let out = self.run(
            repo,
            &["diff", "--name-only", "-z", "--no-renames"],
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

/// Added plus deleted lines in `git diff --numstat`; a binary file (`-`) counts
/// 0 and an empty diff is 0. Numbers, not words, so the locale does not matter.
fn parse_numstat(out: &str) -> u64 {
    out.lines()
        .filter_map(|line| {
            let mut cols = line.split('\t');
            let added = cols.next()?.parse::<u64>().unwrap_or(0);
            let deleted = cols.next()?.parse::<u64>().unwrap_or(0);
            Some(added + deleted)
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
    fn numstat_sums_added_and_deleted() {
        assert_eq!(parse_numstat(""), 0);
        assert_eq!(parse_numstat("3\t0\ta.rs\n"), 3);
        assert_eq!(parse_numstat("10\t2\ta.rs\n1\t1\tb.rs\n"), 14);
        // Binary files show `-`.
        assert_eq!(parse_numstat("-\t-\timg.png\n4\t1\ta.rs\n"), 5);
    }

    fn repos_at(dir: &Path) -> Repos {
        Repos(BTreeMap::from([(
            "r".to_string(),
            Tip {
                dir: dir.to_path_buf(),
                rev: "main".into(),
            },
        )]))
    }

    fn init() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        tmp
    }

    #[test]
    fn counts_come_from_git_with_and_without_paths() {
        let tmp = init();
        let dir = tmp.path();
        commit(dir, "src/a.rs", "one\n");
        let base = head(dir);
        commit(dir, "src/a.rs", "one\ntwo\nthree\n");
        commit(dir, "docs/d.md", "x\ny\n");
        let repos = repos_at(dir);

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

    #[test]
    fn a_base_that_is_not_hex_is_no_answer_not_an_option() {
        let tmp = init();
        commit(tmp.path(), "a", "x\n");
        let repos = repos_at(tmp.path());
        for base in ["--all", "-n1", "", "main", "HEAD~1", "abc def"] {
            assert_eq!(repos.commits_since("r", base, &[]), None, "{base:?}");
            assert_eq!(repos.lines_since("r", base, &[]), None, "{base:?}");
            assert_eq!(repos.changed_files("r", base), None, "{base:?}");
        }
    }

    #[test]
    fn paths_are_plain_prefixes_not_pathspec_magic() {
        let tmp = init();
        let dir = tmp.path();
        commit(dir, "src/a.rs", "one\n");
        let base = head(dir);
        commit(dir, "src/a.rs", "one\ntwo\n");
        let repos = repos_at(dir);
        // `*` and `:(exclude)` would match or exclude if read as magic.
        assert_eq!(repos.commits_since("r", &base, &["*".into()]), Some(0));
        assert_eq!(
            repos.commits_since("r", &base, &["src/*.rs".into()]),
            Some(0)
        );
        assert_eq!(
            repos.commits_since("r", &base, &[":(exclude)src/".into()]),
            Some(0)
        );
        assert_eq!(repos.commits_since("r", &base, &["src/".into()]), Some(1));
    }

    #[test]
    fn deleted_renamed_and_spaced_files_are_listed() {
        let tmp = init();
        let dir = tmp.path();
        commit(dir, "gone.rs", "bye\n");
        commit(dir, "old.rs", "one\ntwo\nthree\nfour\n");
        commit(dir, "my dir/it's here.rs", "x\n");
        let base = head(dir);
        git(dir, &["rm", "-q", "gone.rs"]);
        git(dir, &["mv", "old.rs", "new.rs"]);
        commit(dir, "my dir/it's here.rs", "x\ny\n");
        let repos = repos_at(dir);

        let mut files = repos.changed_files("r", &base).unwrap();
        files.sort();
        assert_eq!(
            files,
            ["gone.rs", "my dir/it's here.rs", "new.rs", "old.rs"],
            "a rename shows both paths"
        );
        assert_eq!(repos.lines_since("r", &base, &["my dir/".into()]), Some(1));
    }

    #[test]
    fn a_binary_file_counts_no_lines() {
        let tmp = init();
        let dir = tmp.path();
        commit(dir, "a.txt", "x\n");
        let base = head(dir);
        std::fs::write(dir.join("img.bin"), [0u8, 159, 146, 150, 0]).unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-qm", "bin"]);
        assert_eq!(repos_at(dir).lines_since("r", &base, &[]), Some(0));
    }
}
