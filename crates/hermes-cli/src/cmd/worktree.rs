//! `hermes worktree` — audit and reclaim git worktrees Hermes sessions accumulate.
//!
//! Ported from NousResearch/hermes-agent's `hermes_cli/worktree_cmd.py`
//! (plus the audit semantics of `worktree_gc.py`).
//!
//! `list` (default) classifies every worktree — age, size, verdict, reason.
//! `prune` archives the branch tip of safe trees to `refs/hermes/archive/<name>`
//! before removing them, and deletes fully-merged local branches.
//! `restore` recreates a worktree from an archived ref.
//! Trees that are locked, dirty, detached, young, or carry unmerged work are
//! never touched. `--dry-run` shows the plan without changing anything.

use std::path::{Path, PathBuf};
use std::process::Command;

/// CLI args for `hermes worktree`.
#[derive(clap::Args, Debug, Clone)]
pub struct WorktreeArgs {
    /// Action: list (default), prune, restore.
    pub action: Option<String>,
    /// Repo root (default: the git repo containing the current directory).
    #[arg(long)]
    pub repo: Option<String>,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
    /// Treat reapable trees younger than DAYS as keep.
    #[arg(long)]
    pub older_than: Option<f64>,
    /// Show the plan without changing anything (prune).
    #[arg(long)]
    pub dry_run: bool,
    /// Only remove worktrees; leave local branches alone (prune).
    #[arg(long)]
    pub trees_only: bool,
    /// Only delete merged local branches; leave worktrees alone (prune).
    #[arg(long)]
    pub branches_only: bool,
    /// Archive name for `restore` (see `hermes worktree list`).
    #[arg(long)]
    pub name: Option<String>,
    /// Restore here instead of the original path.
    #[arg(long)]
    pub path: Option<String>,
}

/// One audited worktree.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub name: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: String,
    pub detached: bool,
    pub locked: bool,
    pub bare: bool,
    pub is_main: bool,
    pub age_days: f64,
    pub size_mb: Option<u64>,
    pub dirty: bool,
    pub verdict: String,
    pub reason: String,
}

/// One audited local branch.
#[derive(Debug, Clone)]
pub struct BranchInfo {
    pub name: String,
    pub merged: bool,
    pub checked_out: bool,
    pub verdict: String,
}

/// Run `git <args>` in `repo`; `None` on any failure.
fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Find the repo root containing the current directory.
fn discover_repo_root() -> Option<PathBuf> {
    git(
        &std::env::current_dir().ok()?,
        &["rev-parse", "--show-toplevel"],
    )
    .map(PathBuf::from)
}

/// Parse `git worktree list --porcelain` into raw records.
fn parse_worktree_porcelain(text: &str) -> Vec<WorktreeInfo> {
    let mut out = Vec::new();
    let mut cur: Option<WorktreeInfo> = None;
    let flush = |cur: &mut Option<WorktreeInfo>, out: &mut Vec<WorktreeInfo>| {
        if let Some(w) = cur.take() {
            out.push(w);
        }
    };
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            flush(&mut cur, &mut out);
            let path = PathBuf::from(path);
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            cur = Some(WorktreeInfo {
                name,
                path,
                branch: None,
                head: String::new(),
                detached: false,
                locked: false,
                bare: false,
                is_main: false,
                age_days: 0.0,
                size_mb: None,
                dirty: false,
                verdict: String::new(),
                reason: String::new(),
            });
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            if let Some(w) = cur.as_mut() {
                w.head = head.to_string();
            }
        } else if let Some(branch) = line.strip_prefix("branch ") {
            if let Some(w) = cur.as_mut() {
                w.branch = Some(
                    branch
                        .strip_prefix("refs/heads/")
                        .unwrap_or(branch)
                        .to_string(),
                );
            }
        } else if line == "detached" {
            if let Some(w) = cur.as_mut() {
                w.detached = true;
            }
        } else if line.starts_with("locked") {
            if let Some(w) = cur.as_mut() {
                w.locked = true;
            }
        } else if line == "bare" {
            if let Some(w) = cur.as_mut() {
                w.bare = true;
            }
        }
    }
    flush(&mut cur, &mut out);
    out
}

fn du_size_mb(path: &Path) -> Option<u64> {
    let out = Command::new("du")
        .args(["-sm", "--"])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn is_dirty(path: &Path) -> bool {
    Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(path)
        .output()
        .map(|o| o.status.success() && !o.stdout.iter().all(|b| b.is_ascii_whitespace()))
        .unwrap_or(false)
}

/// Last-commit age of the worktree's HEAD, in days (dir mtime fallback).
fn head_age_days(path: &Path) -> f64 {
    let ts: Option<f64> = git(path, &["log", "-1", "--format=%ct"])
        .and_then(|s| s.parse().ok())
        .or_else(|| {
            std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| {
                    t.duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_secs_f64())
                })
        });
    match ts {
        Some(t) => ((std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(t)
            - t)
            / 86400.0)
            .max(0.0),
        None => 0.0,
    }
}

/// Default branch name (origin/HEAD, else local main/master).
fn default_branch(repo: &Path) -> Option<String> {
    if let Some(r) = git(repo, &["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        return r.rsplit('/').next().map(|s| s.to_string());
    }
    for b in ["main", "master"] {
        if git(repo, &["rev-parse", "--verify", &format!("refs/heads/{b}")]).is_some() {
            return Some(b.to_string());
        }
    }
    None
}

fn branch_merged_into(repo: &Path, branch: &str, base: &str) -> bool {
    Command::new("git")
        .args(["merge-base", "--is-ancestor", branch, base])
        .current_dir(repo)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Audit every worktree of `repo`, assigning verdict + reason.
pub fn audit_worktrees(repo: &Path, older_than_days: Option<f64>) -> Vec<WorktreeInfo> {
    let raw = git(repo, &["worktree", "list", "--porcelain"]).unwrap_or_default();
    let mut trees = parse_worktree_porcelain(&raw);
    let main_path = git(repo, &["rev-parse", "--show-toplevel"]).map(PathBuf::from);
    let older_than = older_than_days.unwrap_or(7.0).max(0.0);
    let default = default_branch(repo);

    for (i, w) in trees.iter_mut().enumerate() {
        if i == 0 || main_path.as_ref().map(|p| p == &w.path).unwrap_or(false) {
            w.is_main = true;
        }
        w.age_days = head_age_days(&w.path);
        w.size_mb = du_size_mb(&w.path);
        w.dirty = !w.bare && is_dirty(&w.path);

        let (verdict, reason) = if w.is_main {
            ("keep", "main worktree")
        } else if w.bare {
            ("keep", "bare repository")
        } else if w.locked {
            ("keep", "locked / in use")
        } else if w.dirty {
            ("keep", "uncommitted changes")
        } else if w.detached {
            ("keep", "detached HEAD")
        } else if w.age_days < older_than {
            ("keep", "younger than --older-than")
        } else {
            match (&w.branch, &default) {
                (Some(branch), Some(base)) if branch_merged_into(repo, branch, base) => {
                    ("reap", "branch fully merged")
                }
                (Some(_), _) => ("keep", "branch has unmerged work"),
                (None, _) => ("keep", "no branch info"),
            }
        };
        w.verdict = verdict.to_string();
        w.reason = reason.to_string();
    }
    trees
}

/// Audit local branches: fully-merged, not checked out anywhere → delete.
pub fn audit_branches(repo: &Path) -> Vec<BranchInfo> {
    let mut out = Vec::new();
    let default = match default_branch(repo) {
        Some(b) => b,
        None => return out,
    };
    let merged = git(
        repo,
        &["branch", "--merged", &default, "--format=%(refname:short)"],
    )
    .unwrap_or_default();
    let all = git(repo, &["branch", "--format=%(refname:short)"]).unwrap_or_default();
    let checked_out: Vec<String> = parse_worktree_porcelain(
        &git(repo, &["worktree", "list", "--porcelain"]).unwrap_or_default(),
    )
    .into_iter()
    .filter_map(|w| w.branch)
    .collect();
    for name in all.lines().map(str::trim).filter(|s| !s.is_empty()) {
        if name == default {
            continue;
        }
        let is_merged = merged.lines().map(str::trim).any(|m| m == name);
        let in_use = checked_out.iter().any(|b| b == name);
        let verdict = if is_merged && !in_use {
            "delete"
        } else {
            "keep"
        };
        out.push(BranchInfo {
            name: name.to_string(),
            merged: is_merged,
            checked_out: in_use,
            verdict: verdict.to_string(),
        });
    }
    out
}

/// Archive refs under `refs/hermes/archive/`.
pub fn list_archives(repo: &Path) -> Vec<String> {
    git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/hermes/archive/",
        ],
    )
    .unwrap_or_default()
    .lines()
    .map(|l| {
        l.strip_prefix("refs/hermes/archive/")
            .unwrap_or(l)
            .to_string()
    })
    .filter(|s| !s.is_empty())
    .collect()
}

fn sanitize_archive_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn fmt_size(size_mb: Option<u64>) -> String {
    match size_mb {
        None => "?".to_string(),
        Some(mb) if mb >= 1024 => format!("{:.1}G", mb as f64 / 1024.0),
        Some(mb) => format!("{mb}M"),
    }
}

fn print_list(
    repo: &Path,
    trees: &[WorktreeInfo],
    branches: &[BranchInfo],
    as_json: bool,
) -> anyhow::Result<()> {
    if as_json {
        let doc = serde_json::json!({
            "repo": repo.display().to_string(),
            "trees": trees.iter().map(|w| serde_json::json!({
                "name": w.name, "path": w.path.display().to_string(),
                "branch": w.branch, "head": w.head, "detached": w.detached,
                "locked": w.locked, "age_days": w.age_days, "size_mb": w.size_mb,
                "dirty": w.dirty, "verdict": w.verdict, "reason": w.reason,
            })).collect::<Vec<_>>(),
            "branches": branches.iter().map(|b| serde_json::json!({
                "name": b.name, "merged": b.merged,
                "checked_out": b.checked_out, "verdict": b.verdict,
            })).collect::<Vec<_>>(),
            "archives": list_archives(repo),
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(());
    }
    let archives = list_archives(repo);
    if !archives.is_empty() {
        println!(
            "{} archived worktree(s) (restore with `hermes worktree restore --name <name>`):",
            archives.len()
        );
        for a in &archives {
            println!("  {a}");
        }
        println!();
    }
    if trees.is_empty() {
        println!("No worktrees — nothing to reclaim.");
        return Ok(());
    }
    let total: u64 = trees.iter().filter_map(|w| w.size_mb).sum();
    let reapable: u64 = trees
        .iter()
        .filter(|w| w.verdict == "reap")
        .filter_map(|w| w.size_mb)
        .sum();
    println!(
        "{:<32} {:>6} {:>6} {:<13} REASON",
        "TREE", "AGE", "SIZE", "VERDICT"
    );
    let mut sorted = trees.to_vec();
    sorted.sort_by_key(|a| std::cmp::Reverse(a.size_mb));
    for w in &sorted {
        println!(
            "{:<32} {:>5.1}d {:>6} {:<13} {}",
            w.name.chars().take(32).collect::<String>(),
            w.age_days,
            fmt_size(w.size_mb),
            w.verdict,
            w.reason
        );
    }
    println!(
        "\n{} tree(s), {} total — {} reclaimable now via `hermes worktree prune`.",
        trees.len(),
        fmt_size(Some(total)),
        fmt_size(Some(reapable))
    );
    let deletable: Vec<_> = branches.iter().filter(|b| b.verdict == "delete").collect();
    if !deletable.is_empty() {
        println!(
            "{} local branch(es) fully merged would also be deleted by prune.",
            deletable.len()
        );
    }
    Ok(())
}

fn run_list(repo: &Path, args: &WorktreeArgs) -> anyhow::Result<()> {
    let trees = audit_worktrees(repo, args.older_than);
    let branches = audit_branches(repo);
    print_list(repo, &trees, &branches, args.json)
}

fn run_prune(repo: &Path, args: &WorktreeArgs) -> anyhow::Result<()> {
    let dry_run = args.dry_run;
    let mut actions: Vec<String> = Vec::new();

    if !args.branches_only {
        let trees = audit_worktrees(repo, args.older_than);
        for w in trees.iter().filter(|w| w.verdict == "reap") {
            let archive_name = sanitize_archive_name(&w.name);
            let archive_ref = format!("refs/hermes/archive/{archive_name}");
            if dry_run {
                actions.push(format!(
                    "would archive {} → {archive_ref} and remove {}",
                    w.name,
                    w.path.display()
                ));
                continue;
            }
            // Archive the branch tip first so the work is recoverable.
            if !w.head.is_empty() && git(repo, &["update-ref", &archive_ref, &w.head]).is_none() {
                actions.push(format!("FAILED to archive {} (skipping removal)", w.name));
                continue;
            }
            match Command::new("git")
                .args(["worktree", "remove", "--force", "--"])
                .arg(&w.path)
                .current_dir(repo)
                .output()
            {
                Ok(o) if o.status.success() => actions.push(format!(
                    "archived {} → {archive_ref}, removed {}",
                    w.name,
                    w.path.display()
                )),
                _ => actions.push(format!("FAILED to remove worktree {}", w.path.display())),
            }
        }
    }

    if !args.trees_only {
        let branches = audit_branches(repo);
        for b in branches.iter().filter(|b| b.verdict == "delete") {
            if dry_run {
                actions.push(format!("would delete merged branch {}", b.name));
                continue;
            }
            match Command::new("git")
                .args(["branch", "-d", &b.name])
                .current_dir(repo)
                .output()
            {
                Ok(o) if o.status.success() => {
                    actions.push(format!("deleted merged branch {}", b.name));
                }
                _ => actions.push(format!("FAILED to delete branch {}", b.name)),
            }
        }
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "repo": repo.display().to_string(),
                "dry_run": dry_run,
                "actions": actions,
            }))?
        );
        return Ok(());
    }
    if actions.is_empty() {
        println!("Nothing to reclaim — all trees/branches carry real work or are in use.");
    } else {
        for a in &actions {
            println!("  {a}");
        }
        println!(
            "{} action(s) {}.",
            actions.len(),
            if dry_run { "planned" } else { "done" }
        );
    }
    Ok(())
}

fn run_restore(repo: &Path, args: &WorktreeArgs) -> anyhow::Result<()> {
    let name = args.name.as_deref().ok_or_else(|| {
        anyhow::anyhow!("Missing archive name. Usage: hermes worktree restore --name <name>")
    })?;
    let archive_ref = format!("refs/hermes/archive/{name}");
    let sha = git(repo, &["rev-parse", "--verify", &archive_ref])
        .ok_or_else(|| anyhow::anyhow!("No archive '{name}' (see `hermes worktree list`)."))?;
    let path = match &args.path {
        Some(p) => PathBuf::from(p),
        None => repo.join(".worktrees").join(name),
    };
    if path.exists() {
        return Err(anyhow::anyhow!(
            "Restore path {} already exists. Pass --path <dir> to choose another.",
            path.display()
        ));
    }
    let branch = format!("restored/{name}");
    let status = Command::new("git")
        .args(["worktree", "add", "-b", &branch, "--"])
        .arg(&path)
        .arg(&sha)
        .current_dir(repo)
        .status()
        .map_err(|e| anyhow::anyhow!("git worktree add failed: {e}"))?;
    if !status.success() {
        return Err(anyhow::anyhow!(
            "git worktree add failed for archive '{name}'."
        ));
    }
    println!("✓ Restored {archive_ref} → {}", path.display());
    println!("  Branch: {branch} (at {})", &sha[..12.min(sha.len())]);
    println!("  Note: only committed work is restored; untracked files were never archived.");
    Ok(())
}

/// Dispatch `hermes worktree`.
pub async fn run(args: &WorktreeArgs) -> anyhow::Result<()> {
    let repo = match &args.repo {
        Some(r) => PathBuf::from(r),
        None => discover_repo_root().ok_or_else(|| {
            anyhow::anyhow!("Not inside a git repository (or pass --repo <path>).")
        })?,
    };
    if !repo.join(".git").exists() && git(&repo, &["rev-parse", "--git-dir"]).is_none() {
        return Err(anyhow::anyhow!(
            "{} is not a git repository.",
            repo.display()
        ));
    }
    if let Some(days) = args.older_than {
        if days < 0.0 {
            return Err(anyhow::anyhow!(
                "--older-than must be a non-negative number of days."
            ));
        }
    }
    match args.action.as_deref().unwrap_or("list") {
        "list" | "ls" | "audit" => run_list(&repo, args),
        "prune" => run_prune(&repo, args),
        "restore" => run_restore(&repo, args),
        other => {
            println!("Unknown worktree action: '{other}'. Use: list, prune, restore.");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn init_repo(name: &str) -> Option<PathBuf> {
        if !git_available() {
            return None;
        }
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok()?;
        for a in [
            ["init", "-b", "main"].as_slice(),
            ["config", "user.email", "t@t"].as_slice(),
            ["config", "user.name", "t"].as_slice(),
        ] {
            Command::new("git")
                .args(a)
                .current_dir(&dir)
                .output()
                .ok()?;
        }
        std::fs::write(dir.join("f.txt"), "hi\n").ok()?;
        Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .ok()?;
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&dir)
            .output()
            .ok()?;
        Some(dir)
    }

    #[test]
    fn parse_worktree_porcelain_handles_all_fields() {
        let text = "worktree /repo\nHEAD abc123\nbranch refs/heads/main\n\nworktree /repo/wt\nHEAD def456\nbranch refs/heads/feat\nlocked\n\nworktree /repo/det\nHEAD 789abc\ndetached\nbare\n";
        let trees = parse_worktree_porcelain(text);
        assert_eq!(trees.len(), 3);
        assert_eq!(trees[0].branch.as_deref(), Some("main"));
        assert_eq!(trees[0].head, "abc123");
        assert!(trees[1].locked);
        assert_eq!(trees[1].branch.as_deref(), Some("feat"));
        assert!(trees[2].detached && trees[2].bare);
    }

    #[test]
    fn sanitize_archive_name_strips_unsafe_chars() {
        assert_eq!(
            sanitize_archive_name("feat/my-branch.1"),
            "feat_my-branch.1"
        );
        assert_eq!(sanitize_archive_name("a b"), "a_b");
    }

    #[test]
    fn fmt_size_formats_mb_and_gb() {
        assert_eq!(fmt_size(None), "?");
        assert_eq!(fmt_size(Some(512)), "512M");
        assert_eq!(fmt_size(Some(2048)), "2.0G");
    }

    #[test]
    fn audit_keeps_main_and_reaps_merged() {
        let Some(dir) = init_repo("hermes-worktree-test") else {
            return;
        };
        // Merged branch worktree.
        Command::new("git")
            .args(["checkout", "-b", "feat"])
            .current_dir(&dir)
            .output()
            .unwrap();
        std::fs::write(dir.join("g.txt"), "x\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "feat"])
            .current_dir(&dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["checkout", "main"])
            .current_dir(&dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["merge", "--ff-only", "feat"])
            .current_dir(&dir)
            .output()
            .unwrap();
        let wt = dir.join("wt-feat");
        Command::new("git")
            .args(["worktree", "add", "--"])
            .arg(&wt)
            .arg("feat")
            .current_dir(&dir)
            .output()
            .unwrap();

        // A second merged branch that is NOT checked out anywhere.
        Command::new("git")
            .args(["checkout", "-b", "feat2"])
            .current_dir(&dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["checkout", "main"])
            .current_dir(&dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["merge", "--ff-only", "feat2"])
            .current_dir(&dir)
            .output()
            .unwrap();

        let trees = audit_worktrees(&dir, Some(0.0));
        assert!(trees.iter().any(|w| w.is_main && w.verdict == "keep"));
        let feat = trees
            .iter()
            .find(|w| w.name == "wt-feat")
            .expect("worktree listed");
        assert_eq!(
            feat.verdict, "reap",
            "merged clean tree should reap: {:?}",
            feat
        );
        assert!(!feat.dirty);

        let branches = audit_branches(&dir);
        // feat is checked out in the worktree → kept even though merged.
        let feat_branch = branches
            .iter()
            .find(|b| b.name == "feat")
            .expect("branch listed");
        assert_eq!(feat_branch.verdict, "keep");
        assert!(feat_branch.checked_out);
        // feat2 is merged and checked out nowhere → deletable.
        let feat2_branch = branches
            .iter()
            .find(|b| b.name == "feat2")
            .expect("branch listed");
        assert_eq!(feat2_branch.verdict, "delete");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn audit_keeps_dirty_worktree() {
        let Some(dir) = init_repo("hermes-worktree-test-dirty") else {
            return;
        };
        Command::new("git")
            .args(["checkout", "-b", "dirty-branch"])
            .current_dir(&dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["checkout", "main"])
            .current_dir(&dir)
            .output()
            .unwrap();
        let wt = dir.join("wt-dirty");
        Command::new("git")
            .args(["worktree", "add", "--"])
            .arg(&wt)
            .arg("dirty-branch")
            .current_dir(&dir)
            .output()
            .unwrap();
        std::fs::write(wt.join("uncommitted.txt"), "work in progress\n").unwrap();

        let trees = audit_worktrees(&dir, Some(0.0));
        let dirty = trees
            .iter()
            .find(|w| w.name == "wt-dirty")
            .expect("worktree listed");
        assert_eq!(dirty.verdict, "keep");
        assert!(dirty.dirty);
        assert_eq!(dirty.reason, "uncommitted changes");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
