//! Git integration by shelling out to the `git` CLI, so the user's config,
//! credentials and hooks all behave exactly as on the command line.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug)]
pub struct Repo {
    pub root: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileStatus {
    /// Path relative to the repo root.
    pub path: String,
    /// Index (staged) status letter, if staged.
    pub staged: Option<char>,
    /// Worktree status letter, if changed and unstaged ('?' for untracked).
    pub unstaged: Option<char>,
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
    pub head: String,
    pub files: Vec<FileStatus>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LineMark {
    Added,
    Modified,
    /// Lines were deleted just above this line.
    Deleted,
}

impl Repo {
    pub fn discover(dir: &Path) -> Option<Repo> {
        let out = Command::new("git").arg("-C").arg(dir).args(["rev-parse", "--show-toplevel"]).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Some(Repo { root: PathBuf::from(root) })
    }

    pub fn init(dir: &Path) -> Result<Repo, String> {
        run_in(dir, &["init"])?;
        Repo::discover(dir).ok_or_else(|| "git init failed".into())
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new("git");
        c.arg("-C").arg(&self.root).env("GIT_TERMINAL_PROMPT", "0").env("GIT_OPTIONAL_LOCKS", "0");
        c
    }

    pub fn run(&self, args: &[&str]) -> Result<String, String> {
        let out = self.cmd().args(args).output().map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let msg = if err.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { err };
            Err(msg)
        }
    }

    pub fn rel(&self, path: &Path) -> Option<String> {
        let p = path.strip_prefix(&self.root).ok()?;
        Some(p.to_string_lossy().replace('\\', "/"))
    }

    pub fn status(&self) -> Result<Status, String> {
        let out = self.run(&["status", "--porcelain=v1", "-z", "--branch", "--untracked-files=all"])?;
        let mut st = Status::default();
        let mut parts = out.split('\0');
        while let Some(entry) = parts.next() {
            if entry.len() < 3 {
                continue;
            }
            if let Some(b) = entry.strip_prefix("## ") {
                parse_branch(b, &mut st);
                continue;
            }
            let x = entry.as_bytes()[0] as char;
            let y = entry.as_bytes()[1] as char;
            let path = entry[3..].to_string();
            // Renames and copies are followed by the original path, staged (X) or not (Y).
            if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
                parts.next(); // original path
            }
            let (staged, unstaged) = if x == '?' {
                (None, Some('?'))
            } else {
                ((x != ' ').then_some(x), (y != ' ').then_some(y))
            };
            st.files.push(FileStatus { path, staged, unstaged });
        }
        st.head = self.run(&["rev-parse", "HEAD"]).map(|s| s.trim().to_string()).unwrap_or_default();
        Ok(st)
    }

    pub fn stage(&self, path: &str) -> Result<String, String> {
        self.run(&["add", "--", path])
    }

    pub fn unstage(&self, path: &str) -> Result<String, String> {
        // `reset` also works in a repo without commits, unlike `restore --staged`.
        self.run(&["reset", "-q", "--", path])
    }

    pub fn stage_all(&self) -> Result<String, String> {
        self.run(&["add", "-A"])
    }

    pub fn unstage_all(&self) -> Result<String, String> {
        self.run(&["reset", "-q"])
    }

    /// Throw away worktree changes of a file (deletes untracked files).
    pub fn discard(&self, f: &FileStatus) -> Result<String, String> {
        if f.unstaged == Some('?') {
            std::fs::remove_file(self.root.join(&f.path)).map_err(|e| e.to_string())?;
            Ok(String::new())
        } else {
            self.run(&["checkout", "--", &f.path])
        }
    }

    /// Whether the index differs from HEAD (also true in a repo without commits that has staged files).
    pub fn has_staged(&self) -> bool {
        match self.cmd().args(["diff", "--cached", "--quiet"]).status() {
            Ok(st) => st.code() == Some(1),
            Err(_) => true,
        }
    }

    pub fn commit(&self, message: &str) -> Result<String, String> {
        self.run(&["commit", "-m", message])
    }

    pub fn branches(&self) -> Vec<String> {
        self.run(&["branch", "--format=%(refname:short)"])
            // A detached HEAD shows up as "(HEAD detached at …)", which isn't a branch you can check out.
            .map(|s| s.lines().filter(|l| !l.starts_with('(')).map(|l| l.to_string()).collect())
            .unwrap_or_default()
    }

    pub fn checkout(&self, branch: &str) -> Result<String, String> {
        self.run(&["checkout", branch])
    }

    pub fn create_branch(&self, branch: &str) -> Result<String, String> {
        self.run(&["checkout", "-b", branch])
    }

    /// Contents of a file at HEAD, or None if it isn't tracked there.
    pub fn head_content(&self, rel: &str) -> Option<String> {
        self.run(&["show", &format!("HEAD:{rel}")]).ok()
    }

    pub fn diff(&self, f: &FileStatus, staged: bool) -> String {
        let result = if f.unstaged == Some('?') && !staged {
            // Untracked: diff against /dev/null; git exits 1 when there are differences.
            let out = self.cmd().args(["diff", "--no-color", "--no-index", "--", "/dev/null", &f.path]).output();
            out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).map_err(|e| e.to_string())
        } else if staged {
            self.run(&["diff", "--no-color", "--cached", "--", &f.path])
        } else {
            self.run(&["diff", "--no-color", "--", &f.path])
        };
        result.unwrap_or_else(|e| e)
    }

    pub fn log(&self) -> String {
        self.run(&["log", "--graph", "--decorate", "--date=relative", "--format=%h %d %s  (%an, %ad)", "-n", "300"])
            .unwrap_or_else(|e| e)
    }

    /// Network operations run on a background thread; this is the blocking part.
    /// Run a network command: never prompts (a hidden password prompt would hang forever)
    /// and gives up after a few minutes.
    fn run_remote(&self, args: &[&str]) -> Result<String, String> {
        let mut c = self.cmd();
        // ssh would ask for a passphrase on the terminal rim was started from. Only override when
        // the user hasn't configured their own ssh command.
        let custom_ssh = std::env::var_os("GIT_SSH_COMMAND").is_some() || std::env::var_os("GIT_SSH").is_some()
            || self.run(&["config", "--get", "core.sshCommand"]).is_ok_and(|s| !s.trim().is_empty());
        if !custom_ssh {
            c.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
        }
        let child = c.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
        let out = crate::format::wait_with_timeout(child, std::time::Duration::from_secs(300))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(if err.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { err })
        }
    }

    pub fn remote_op(root: PathBuf, op: &str) -> Result<String, String> {
        let repo = Repo { root };
        let res = match op {
            "pull" => repo.run_remote(&["pull", "--no-rebase"]),
            "push" => repo.run_remote(&["push"]).or_else(|e| {
                if e.contains("has no upstream branch") {
                    let branch = repo.run(&["branch", "--show-current"])?.trim().to_string();
                    repo.run_remote(&["push", "--set-upstream", "origin", &branch])
                } else {
                    Err(e)
                }
            }),
            "fetch" => repo.run_remote(&["fetch", "--all", "--prune"]),
            _ => Err(format!("unknown op {op}")),
        };
        // git prints progress on stderr; success output is often empty.
        res.map(|s| if s.trim().is_empty() { format!("git {op}: done") } else { s })
    }
}

fn run_in(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

/// Parses "main...origin/main [ahead 1, behind 2]" or "No commits yet on main".
fn parse_branch(s: &str, st: &mut Status) {
    let s = s.strip_prefix("No commits yet on ").unwrap_or(s);
    let (head, rest) = s.split_once(' ').unwrap_or((s, ""));
    st.branch = head.split("...").next().unwrap_or(head).to_string();
    if let Some(n) = rest.split("ahead ").nth(1) {
        st.ahead = n.trim_end_matches(']').split(',').next().unwrap_or("0").trim().parse().unwrap_or(0);
    }
    if let Some(n) = rest.split("behind ").nth(1) {
        st.behind = n.trim_end_matches(']').trim().parse().unwrap_or(0);
    }
}

/// Per-line change markers comparing the buffer against HEAD.
pub fn line_marks(base: &str, text: &str) -> Vec<(usize, usize, LineMark)> {
    use similar::{Algorithm, DiffOp, TextDiff};
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .timeout(std::time::Duration::from_millis(40))
        .diff_lines(base, text);
    let mut marks = Vec::new();
    for op in diff.ops() {
        match *op {
            DiffOp::Insert { new_index, new_len, .. } => marks.push((new_index, new_index + new_len, LineMark::Added)),
            DiffOp::Replace { new_index, new_len, .. } => {
                marks.push((new_index, new_index + new_len, LineMark::Modified))
            }
            DiffOp::Delete { new_index, .. } => marks.push((new_index, new_index + 1, LineMark::Deleted)),
            DiffOp::Equal { .. } => {}
        }
    }
    marks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_line() {
        let mut st = Status::default();
        parse_branch("main...origin/main [ahead 2, behind 3]", &mut st);
        assert_eq!((st.branch.as_str(), st.ahead, st.behind), ("main", 2, 3));
        let mut st = Status::default();
        parse_branch("No commits yet on dev", &mut st);
        assert_eq!(st.branch, "dev");
    }

    #[test]
    fn marks() {
        let m = line_marks("a\nb\nc\n", "a\nB\nc\nd\n");
        assert_eq!(m, vec![(1, 2, LineMark::Modified), (3, 4, LineMark::Added)]);
    }
}

#[cfg(test)]
#[path = "validate/git_tests.rs"]
pub(crate) mod validate_tests;
