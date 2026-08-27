// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Hotfix branch and release tooling.
//!
//! A hotfix is a point-in-time patch applied to an already-released line.
//! Releases are tagged `v<X.Y.Z>` and hotfixes on top of release `X.Y.Z` are
//! versioned `X.Y.Z-hotfix.<N>` (tagged `vX.Y.Z-hotfix.<N>`). This module
//! implements:
//!
//! * [`HotfixVersion`] – a typed view of `X.Y.Z-hotfix.<N>` with parse, format,
//!   and `next()` (auto-increment of the hotfix number).
//! * Git plumbing helpers that enumerate release and hotfix tags and resolve
//!   the next hotfix number for a given base release.
//! * [`run`] – the orchestration that branches off a release tag, cherry-picks
//!   a fix, bumps the version to the next hotfix, commits, tags, and optionally
//!   pushes.
//!
//! The command deliberately *never* auto-resolves cherry-pick conflicts: a
//! failed apply aborts loudly so the developer resolves the merge by hand.
//! Remote mutation (push) is gated behind an explicit `--push` (or `--register`),
//! so a dry review run cannot move the remote by accident.

// ── Version model ──────────────────────────────────────────────────────

use std::fmt;
use std::path::Path;
use std::process::ExitCode;

/// A parsed `X.Y.Z-hotfix.<N>` release version.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HotfixVersion {
    /// The base release, e.g. `0`, `0`, `27` for `0.0.27`.
    base: (u64, u64, u64),
    /// Zero-based hotfix number, e.g. `0` for `-hotfix.1`.
    num: u64,
}

impl HotfixVersion {
    /// Parse `X.Y.Z-hotfix.<N>` where `N >= 1`.
    pub fn parse(input: &str) -> Option<Self> {
        let (base, hotfix) = input.split_once("-hotfix.")?;
        let hotfix_num: u64 = hotfix.parse().ok()?;
        if hotfix_num == 0 {
            return None;
        }
        let mut base_parts = base.split('.');
        let major: u64 = base_parts.next()?.parse().ok()?;
        let minor: u64 = base_parts.next()?.parse().ok()?;
        let patch: u64 = base_parts.next()?.parse().ok()?;
        if base_parts.next().is_some() {
            return None;
        }
        Some(Self {
            base: (major, minor, patch),
            num: hotfix_num,
        })
    }

    /// The base release version as a string, e.g. `0.0.27`.
    pub fn base(&self) -> String {
        format!("{}.{}.{}", self.base.0, self.base.1, self.base.2)
    }
}

impl fmt::Display for HotfixVersion {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "{}-hotfix.{}", self.base(), self.num)
    }
}

/// Validate a git tag as a hotfix tag, e.g. `v0.0.27-hotfix.1`.
fn is_valid_hotfix_tag(tag: &str) -> bool {
    tag.strip_prefix('v')
        .and_then(HotfixVersion::parse)
        .is_some()
}

/// Validate a git tag as a plain release tag, e.g. `v0.0.27` (no hotfix).
fn is_valid_release_tag(tag: &str) -> bool {
    let Some(version) = tag.strip_prefix('v') else {
        return false;
    };
    if version.contains("-hotfix.") {
        return false;
    }
    let mut parts = version.split('.');
    let all_numeric = parts.all(|part| part.parse::<u64>().is_ok());
    all_numeric && version.split('.').count() == 3
}

// ── Git plumbing ───────────────────────────────────────────────────────

/// Read stdout of a git command, run from the workspace root.
fn git_output(args: &[&str]) -> Result<String, String> {
    let root = crate::util::workspace_root()?;
    let mut command = crate::util::build_command("git", args);
    command
        .current_dir(&root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = command
        .output()
        .map_err(|e| format!("failed to run git {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {} failed: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run a git command from `cwd` (tests use a temp repo and pass `None` for
/// the workspace-root default via [`git_output`]).
fn git_output_in(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = crate::util::build_command("git", args);
    command
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = command
        .output()
        .map_err(|e| format!("failed to run git {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {} failed: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// List every release tag (`vX.Y.Z`) and hotfix tag (`vX.Y.Z-hotfix.N`),
/// sorted ascending, from the workspace root.
fn list_tags() -> Result<Vec<String>, String> {
    let root = crate::util::workspace_root()?;
    list_tags_in(&root)
}

/// List release/hotfix tags from an explicit repository root (`cwd`).
fn list_tags_in(cwd: &Path) -> Result<Vec<String>, String> {
    let output = git_output_in(cwd, &["tag", "-l", "v*"])?;
    let mut tags: Vec<String> = output
        .lines()
        .map(str::trim)
        .filter(|t| is_valid_release_tag(t) || is_valid_hotfix_tag(t))
        .map(str::to_string)
        .collect();
    sort_release_tags(&mut tags);
    Ok(tags)
}

/// Order release/hotfix tags by their numeric version (release first, then
/// hotfix number). Mutates `tags` in place.
fn sort_release_tags(tags: &mut [String]) {
    fn rank(tag: &str) -> (u64, u64, u64, bool, u64) {
        let v = tag.strip_prefix('v').unwrap_or(tag);
        if let Some(hv) = HotfixVersion::parse(v) {
            let (maj, min, pat) = hv.base;
            (maj, min, pat, true, hv.num)
        } else {
            let parts: Vec<&str> = v.split('.').collect();
            let maj = parts.first().and_then(|p| p.parse().ok()).unwrap_or(0);
            let min = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(0);
            let pat = parts.get(2).and_then(|p| p.parse().ok()).unwrap_or(0);
            (maj, min, pat, false, 0)
        }
    }
    tags.sort_by_key(|t| rank(t));
}

/// The most recent plain release tag, e.g. `v0.0.27`. Ignores hotfix tags.
fn latest_release_tag() -> Result<String, String> {
    let root = crate::util::workspace_root()?;
    latest_release_tag_in(&root)
}

/// The most recent release tag within an explicit repository root.
fn latest_release_tag_in(root: &Path) -> Result<String, String> {
    let tags = list_tags_in(root)?;
    tags.into_iter()
        .rev()
        .find(|t| is_valid_release_tag(t))
        .ok_or_else(|| "no release tags (vX.Y.Z) found".to_string())
}

/// Release tags strictly newer than `before` up to the latest release,
/// ascending. `before` may be `None` to mean "all release tags".
fn release_tags_since(before: Option<&str>) -> Result<Vec<String>, String> {
    let root = crate::util::workspace_root()?;
    release_tags_since_in(&root, before)
}

/// Release tags strictly newer than `before`, from an explicit repo root.
fn release_tags_since_in(root: &Path, before: Option<&str>) -> Result<Vec<String>, String> {
    let tags = list_tags_in(root)?;
    let start = before.map(|b| b.strip_prefix('v').unwrap_or(b).to_string());
    Ok(tags
        .into_iter()
        .filter(|t| is_valid_release_tag(t))
        .filter(|t| {
            start
                .as_ref()
                .map(|from| t.strip_prefix('v').unwrap_or(t) > from.as_str())
                .unwrap_or(true)
        })
        .collect())
}

/// The highest hotfix number (`N`) already tagged for base `X.Y.Z`, or `0`.
fn existing_hotfix_max_in(root: &Path, base: &str) -> Result<u64, String> {
    let tags = list_tags_in(root)?;
    let prefix = format!("v{base}-hotfix.");
    let mut max = 0u64;
    for tag in tags {
        if let Some(num) = tag.strip_prefix(&prefix) {
            if let Ok(n) = num.parse::<u64>() {
                max = max.max(n);
            }
        }
    }
    Ok(max)
}

/// The next unused hotfix version for a base release, e.g. `0.0.27-hotfix.3`.
fn next_hotfix_for_base(base: &str) -> Result<HotfixVersion, String> {
    let root = crate::util::workspace_root()?;
    next_hotfix_for_base_in(&root, base)
}

/// The next unused hotfix version for a base, from an explicit repo root.
fn next_hotfix_for_base_in(root: &Path, base: &str) -> Result<HotfixVersion, String> {
    let max = existing_hotfix_max_in(root, base)?;
    Ok(HotfixVersion {
        base: parse_base(base)?,
        num: max + 1,
    })
}

/// Parse `X.Y.Z` into a `(major, minor, patch)` tuple.
fn parse_base(base: &str) -> Result<(u64, u64, u64), String> {
    let parts: Vec<&str> = base.split('.').collect();
    if parts.len() != 3 {
        return Err(format!("not a base version: {base}"));
    }
    Ok((
        parts[0]
            .parse()
            .map_err(|_| format!("invalid base version: {base}"))?,
        parts[1]
            .parse()
            .map_err(|_| format!("invalid base version: {base}"))?,
        parts[2]
            .parse()
            .map_err(|_| format!("invalid base version: {base}"))?,
    ))
}

// ── Orchestration ─────────────────────────────────────────────────────

/// Options controlling a hotfix run, parsed from the CLI.
#[derive(Debug)]
pub struct HotfixOptions {
    /// The commit SHA to cherry-pick into each target release.
    pub sha: String,
    /// A single base release to target (e.g. `0.0.27`). Overrides `last_tag`.
    pub tag: Option<String>,
    /// When set, target every release strictly newer than this tag up to the
    /// latest release. Ignored when `tag` is present.
    pub last_tag: Option<String>,
    /// Push branches and tags to `origin`.
    pub push: bool,
    /// Push and print the Azure DevOps pipeline-run instructions.
    pub register: bool,
    /// Dry-run: print the plan without mutating git or the working tree.
    pub dry_run: bool,
}

/// Print the hotfix help text.
pub fn usage() {
    eprintln!(
        "Usage: cargo xtask hotfix <sha> [last-tag] [--tag X.Y.Z] [--push|--register] [--dry-run] [--list]\n\
         \n\
         Cherry-pick <sha> into one or more released lines and cut a `-hotfix.N` release.\n\
         \n\
         Targeting (exactly one applies):\n\
           <sha>                    target only the latest release tag\n\
           --tag X.Y.Z              target one specific base release\n\
           <sha> <last-tag>         target every release tag newer than <last-tag>\n\
         \n\
         Options:\n\
           --push       push the hotfix branch and the vX.Y.Z-hotfix.N tag to origin\n\
           --register   same as --push; prints the Azure DevOps pipeline-run step\n\
           --dry-run    print the plan without touching git or the working tree\n\
           --list       list release and existing hotfix tags, then exit"
    );
}

/// List current release and hotfix tags.
fn run_list() -> ExitCode {
    match list_tags() {
        Ok(tags) if tags.is_empty() => {
            eprintln!(
                "  {} no release or hotfix tags found",
                console::style("✘").yellow()
            );
            ExitCode::FAILURE
        }
        Ok(tags) => {
            for tag in tags {
                eprintln!("  {} {tag}", console::style("•").cyan().bold());
            }
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!(
                "  {} failed to list tags: {message}",
                console::style("✘").red().bold()
            );
            ExitCode::FAILURE
        }
    }
}

/// Entry point for `cargo xtask hotfix ...`.
pub fn run(args: &[&str]) -> ExitCode {
    let root = match crate::util::workspace_root() {
        Ok(root) => root,
        Err(message) => {
            eprintln!("xtask error: {message}");
            return ExitCode::FAILURE;
        }
    };

    // `--list` short-circuits argument parsing entirely.
    if args.contains(&"--list") {
        return run_list();
    }

    let options = match parse_options(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("  {} {message}", console::style("✘").red().bold());
            usage();
            return ExitCode::FAILURE;
        }
    };

    if let Err(message) = ensure_git_repo(&root) {
        eprintln!("  {} {message}", console::style("✘").red().bold());
        return ExitCode::FAILURE;
    }

    if !options.dry_run {
        if let Err(message) = ensure_clean_worktree() {
            eprintln!("  {} {message}", console::style("✘").red().bold());
            return ExitCode::FAILURE;
        }
        if let Err(message) = fetch_tags() {
            eprintln!("  {} {message}", console::style("✘").red().bold());
            return ExitCode::FAILURE;
        }
    }

    // Resolve the set of base releases to hotfix.
    let bases = match target_bases(&options) {
        Ok(bases) => bases,
        Err(message) => {
            eprintln!("  {} {message}", console::style("✘").red().bold());
            return ExitCode::FAILURE;
        }
    };
    if bases.is_empty() {
        eprintln!(
            "  {} no release tags matched the requested target",
            console::style("✘").red().bold()
        );
        return ExitCode::FAILURE;
    }

    eprintln!(
        "\n  {} hotfixing into {} release{}",
        console::style("⚡").cyan().bold(),
        console::style(bases.len()).bold(),
        if bases.len() == 1 { "" } else { "s" },
    );

    let mut first_error: Option<String> = None;
    let mut applied = 0usize;
    let mut dry_plans: Vec<HotfixPlan> = Vec::new();

    for base in &bases {
        match plan_one(base, &options) {
            Ok(plan) => {
                if options.dry_run {
                    dry_plans.push(plan);
                    continue;
                }
                eprintln!(
                    "  {} {} -> {}",
                    console::style("→").cyan().bold(),
                    base,
                    plan.hotfix_version,
                );
                match execute_one(&plan, &options, &root) {
                    Ok(()) => {
                        applied += 1;
                        eprintln!(
                            "  {} hotfix {}{} {}",
                            console::style("✔").green(),
                            plan.hotfix_version,
                            if plan.pushed {
                                " pushed"
                            } else {
                                " tagged locally"
                            },
                            if plan.registered { "(registered)" } else { "" },
                        );
                    }
                    Err(message) => {
                        first_error = Some(message);
                        break;
                    }
                }
            }
            Err(message) => {
                first_error = Some(message);
                break;
            }
        }
    }

    if !dry_plans.is_empty() {
        eprintln!(
            "\n  {} DRY RUN — no changes made:",
            console::style("✦").yellow().bold()
        );
        for plan in &dry_plans {
            eprintln!("    {}", plan.display());
        }
        return ExitCode::SUCCESS;
    }

    if let Some(message) = first_error {
        eprintln!("  {} {message}", console::style("✘").red().bold());
        return ExitCode::FAILURE;
    }

    eprintln!(
        "\n  {} released {} hotfix{}\n",
        console::style("✨").green(),
        console::style(applied).bold(),
        if applied == 1 { "" } else { "es" },
    );
    ExitCode::SUCCESS
}

/// Parse the positional and flag arguments into [`HotfixOptions`].
fn parse_options(args: &[&str]) -> Result<HotfixOptions, String> {
    let (mut tag, mut last_tag) = (None::<String>, None::<String>);
    let mut push = false;
    let mut register = false;
    let mut dry_run = false;

    let mut positionals = Vec::new();
    let mut iter = args.iter().copied();
    while let Some(arg) = iter.next() {
        match arg {
            "--tag" => {
                let value = iter
                    .next()
                    .ok_or("--tag requires a base version, e.g. --tag 0.0.27")?;
                tag = Some(value.to_string());
            }
            "--push" => push = true,
            "--register" => register = true,
            "--dry-run" => dry_run = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown option: {other}"));
            }
            other => positionals.push(other.to_string()),
        }
    }

    if positionals.is_empty() {
        return Err("missing <sha> — the commit to cherry-pick".to_string());
    }
    let sha = positionals.remove(0);
    // An optional second positional is the `last-tag`.
    if positionals.len() > 1 {
        return Err(format!("unexpected extra argument: {}", positionals[1]));
    }
    if let Some(last) = positionals.pop() {
        last_tag = Some(last);
    }

    if !push && !register && !dry_run {
        return Err(
            "use --push, --register, or --dry-run — hotfix would otherwise leave untracked work"
                .to_string(),
        );
    }

    Ok(HotfixOptions {
        sha,
        tag,
        last_tag,
        push,
        register: register || push,
        dry_run,
    })
}

/// Resolve the ordered list of base-release versions to hotfix.
fn target_bases(options: &HotfixOptions) -> Result<Vec<String>, String> {
    if let Some(base) = &options.tag {
        let mut bases = Vec::new();
        let tag = format!("v{base}");
        let exists = git_output(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/tags/{tag}"),
        ])
        .map(|_| true)
        .unwrap_or(false);
        if !exists {
            return Err(format!("release tag {tag} does not exist"));
        }
        bases.push(base.clone());
        return Ok(bases);
    }

    if let Some(last) = &options.last_tag {
        return release_tags_since(Some(last));
    }

    Ok(vec![latest_release_tag()?
        .trim_start_matches('v')
        .to_string()])
}

/// A fully-resolved plan for one base release.
struct HotfixPlan {
    base: String,
    release_tag: String,
    hotfix_version: String,
    hotfix_tag: String,
    branch: String,
    pushed: bool,
    registered: bool,
}

/// Resolve the plan for a single base release without mutating anything.
fn plan_one(base: &str, options: &HotfixOptions) -> Result<HotfixPlan, String> {
    let release_tag = format!("v{base}");
    let next = next_hotfix_for_base(base)?;
    if !options.dry_run {
        // A hotfix branch already exists for this base — refuse a duplicate.
        let branch = format!("hotfix/{base}");
        let exists = git_output(&[
            "branch",
            "--list",
            &branch,
            &format!("remotes/origin/{branch}"),
        ])?;
        if !exists.trim().is_empty() {
            return Err(format!(
                "hotfix branch '{branch}' already exists; resolve/merge it before cutting another hotfix on {base}"
            ));
        }
    }
    let mut plan = HotfixPlan {
        base: base.to_string(),
        release_tag,
        hotfix_version: next.to_string(),
        hotfix_tag: format!("v{next}"),
        branch: format!("hotfix/{base}"),
        pushed: options.push || options.register,
        registered: options.register,
    };
    if options.dry_run {
        plan.pushed = options.push || options.register;
    }
    Ok(plan)
}

/// Render a plan as a single dry-run line (and general progress text).
impl HotfixPlan {
    fn display(&self) -> String {
        format!(
            "{base} -> {version} on {branch} (tag {tag}{push}{register})",
            base = console::style(&self.base).bold(),
            version = console::style(&self.hotfix_version).cyan(),
            branch = console::style(&self.branch).bold(),
            tag = console::style(&self.hotfix_tag).cyan(),
            push = if self.pushed {
                ", pushed to origin"
            } else {
                ""
            },
            register = if self.registered {
                ", pipeline register"
            } else {
                ""
            },
        )
    }
}

/// Execute one plan: branch off the release tag, cherry-pick, bump the
/// version, commit, tag, and optionally push.
fn execute_one(plan: &HotfixPlan, options: &HotfixOptions, root: &Path) -> Result<(), String> {
    let original = git_output(&["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string();
    if original == "HEAD" {
        return Err("detached HEAD — check out a branch before running a hotfix".to_string());
    }

    let run = |args: &[&str]| crate::util::run_command("git", args, Some(root));
    let quiet = |args: &[&str]| crate::util::run_command_quiet("git", args, Some(root));

    // 1. Create the hotfix branch off the release tag's commit.
    run(&[
        "checkout",
        "-b",
        &plan.branch,
        &format!("{}^{{}}", plan.release_tag),
    ])
    .map_err(|e| {
        format!(
            "failed to create branch {} from {}: {e}",
            plan.branch, plan.release_tag
        )
    })?;

    let restore = || {
        let _ = run(&["checkout", &original]);
        let _ = run(&["branch", "-D", &plan.branch]);
    };

    // 2. Cherry-pick the fix (with `-x` to record the source commit). A
    //    conflict aborts loudly; we never auto-resolve.
    if let Err(e) = quiet(&["cherry-pick", "-x", &options.sha]) {
        eprintln!(
            "  {} cherry-pick of {} into {} failed; aborting (resolve conflicts manually if any were staged)",
            console::style("✘").red().bold(),
            options.sha,
            plan.base,
        );
        let _ = quiet(&["cherry-pick", "--abort"]);
        restore();
        return Err(format!("cherry-pick failed: {e}"));
    }

    // 3. Bump every manifest to the next hotfix version.
    let version_exit = crate::version::run(Some(&plan.hotfix_version));
    if version_exit != ExitCode::SUCCESS {
        restore();
        return Err(format!("version bump to {} failed", plan.hotfix_version));
    }

    // 4. Commit the version change.
    run(&["add", "-A"]).map_err(|e| format!("git add failed: {e}"))?;
    run(&[
        "commit",
        "-m",
        &format!("chore: hotfix {}", plan.hotfix_version),
    ])
    .map_err(|e| format!("git commit failed: {e}"))?;

    // 5. Tag the hotfix.
    run(&["tag", &plan.hotfix_tag]).map_err(|e| format!("git tag failed: {e}"))?;

    // 6. Optionally push branch and tag.
    if plan.pushed {
        run(&["push", "origin", &plan.branch])
            .map_err(|e| format!("failed to push branch {}: {e}", plan.branch))?;
        run(&["push", "origin", &plan.hotfix_tag])
            .map_err(|e| format!("failed to push tag {}: {e}", plan.hotfix_tag))?;
        if plan.registered {
            eprintln!(
                "      {} manually run the 'Web UI - CD' pipeline on branch '{}' to sign + publish {}",
                console::style("☞").cyan().bold(),
                plan.branch,
                plan.hotfix_version,
            );
        }
    }

    // Return to the original branch so the workspace stays usable.
    run(&["checkout", &original]).map_err(|e| format!("failed to return to {original}: {e}"))?;

    Ok(())
}

/// Confirm the current directory is inside a git repository.
fn ensure_git_repo(_root: &Path) -> Result<(), String> {
    git_output(&["rev-parse", "--is-inside-work-tree"]).map(|_| ())
}

/// Return an error when the working tree is not clean.
fn ensure_clean_worktree() -> Result<(), String> {
    let status = git_output(&["status", "--porcelain"])?;
    if status.trim().is_empty() {
        Ok(())
    } else {
        Err("working tree is not clean; commit or stash changes before a hotfix (use --dry-run to preview)".to_string())
    }
}

/// Fetch tags from `origin`.
fn fetch_tags() -> Result<(), String> {
    git_output(&["fetch", "origin", "--tags"]).map(|_| ())
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hotfix_version() {
        let hv = HotfixVersion::parse("0.0.27-hotfix.1").unwrap();
        assert_eq!(hv.base(), "0.0.27");
        assert_eq!(hv.to_string(), "0.0.27-hotfix.1");
    }

    #[test]
    fn rejects_invalid_hotfix_versions() {
        assert!(HotfixVersion::parse("0.0.27").is_none());
        assert!(HotfixVersion::parse("0.0.27-hotfix.0").is_none());
        assert!(HotfixVersion::parse("0.0.27-hotfix").is_none());
        assert!(HotfixVersion::parse("0.0.27-hotfix.1.5").is_none());
        assert!(HotfixVersion::parse("hotfix.1").is_none());
        assert!(HotfixVersion::parse("").is_none());
    }

    #[test]
    fn validates_tag_shapes() {
        assert!(is_valid_release_tag("v0.0.27"));
        assert!(!is_valid_release_tag("v0.0.27-hotfix.1"));
        assert!(!is_valid_release_tag("vfoo"));
        assert!(is_valid_hotfix_tag("v0.0.27-hotfix.1"));
        assert!(!is_valid_hotfix_tag("v0.0.27"));
        assert!(!is_valid_hotfix_tag("v0.0.27-hotfix.0"));
    }

    #[test]
    fn next_hotfix_increments_per_base() {
        assert_eq!(
            next_hotfix_for_base("0.0.99").unwrap().to_string(),
            "0.0.99-hotfix.1"
        );
        assert_eq!(
            next_hotfix_for_base("1.2.3").unwrap().to_string(),
            "1.2.3-hotfix.1"
        );
    }

    // -- Git plumbing against an in-memory temp repository -----------------

    /// Init an empty git repo at `dir` with a throwaway commit, tagging it
    /// `vX.Y.Z` (plus any hotfix tags supplied). Returns the head SHA.
    fn init_repo(dir: &Path, tags: &[&str]) {
        git_output_in(dir, &["init", "-q"]).unwrap();
        git_output_in(dir, &["config", "user.email", "test@example.com"]).unwrap();
        git_output_in(dir, &["config", "user.name", "Test"]).unwrap();
        git_output_in(dir, &["config", "commit.gpgsign", "false"]).unwrap();
        std::fs::write(dir.join("file.txt"), "hello").unwrap();
        git_output_in(dir, &["add", "-A"]).unwrap();
        git_output_in(dir, &["commit", "-q", "-m", "init"]).unwrap();
        for tag in tags {
            git_output_in(dir, &["tag", tag]).unwrap();
        }
    }

    #[test]
    fn lists_and_orders_release_and_hotfix_tags() {
        let dir = tempfile::TempDir::new().unwrap();
        init_repo(
            dir.path(),
            &[
                "v0.0.25",
                "v0.0.26",
                "v0.0.27",
                "v0.0.27-hotfix.1",
                "v0.0.27-hotfix.2",
                "v0.0.28",
            ],
        );

        let tags = list_tags_in(dir.path()).unwrap();
        // Release tags first, hotfix tags interleaved by base, ascending.
        assert_eq!(
            tags,
            vec![
                "v0.0.25".to_string(),
                "v0.0.26".to_string(),
                "v0.0.27".to_string(),
                "v0.0.27-hotfix.1".to_string(),
                "v0.0.27-hotfix.2".to_string(),
                "v0.0.28".to_string(),
            ]
        );
    }

    #[test]
    fn latest_release_ignores_hotfix_tags() {
        let dir = tempfile::TempDir::new().unwrap();
        init_repo(dir.path(), &["v0.0.26", "v0.0.27", "v0.0.27-hotfix.1"]);
        assert_eq!(
            latest_release_tag_in(dir.path()).unwrap(),
            "v0.0.27".to_string()
        );
    }

    #[test]
    fn release_tags_since_excludes_hotfix_and_older() {
        let dir = tempfile::TempDir::new().unwrap();
        init_repo(
            dir.path(),
            &[
                "v0.0.25",
                "v0.0.26",
                "v0.0.27",
                "v0.0.27-hotfix.1",
                "v0.0.28",
            ],
        );
        let since = release_tags_since_in(dir.path(), Some("0.0.26")).unwrap();
        assert_eq!(since, vec!["v0.0.27".to_string(), "v0.0.28".to_string()]);
    }

    #[test]
    fn existing_hotfix_max_and_next_increment() {
        let dir = tempfile::TempDir::new().unwrap();
        init_repo(
            dir.path(),
            &["v0.0.27", "v0.0.27-hotfix.1", "v0.0.27-hotfix.2"],
        );
        assert_eq!(existing_hotfix_max_in(dir.path(), "0.0.27").unwrap(), 2);
        let next = next_hotfix_for_base_in(dir.path(), "0.0.27").unwrap();
        assert_eq!(next.to_string(), "0.0.27-hotfix.3");
    }

    #[test]
    fn next_hotfix_for_untagged_base_starts_at_one() {
        let dir = tempfile::TempDir::new().unwrap();
        init_repo(dir.path(), &["v0.0.27", "v0.0.27-hotfix.2"]);
        // Even with a gap (only .2 exists, no .1), we take max+1.
        assert_eq!(
            next_hotfix_for_base_in(dir.path(), "0.0.27")
                .unwrap()
                .to_string(),
            "0.0.27-hotfix.3"
        );

        let dir2 = tempfile::TempDir::new().unwrap();
        init_repo(dir2.path(), &["v0.0.99"]);
        assert_eq!(
            next_hotfix_for_base_in(dir2.path(), "0.0.99")
                .unwrap()
                .to_string(),
            "0.0.99-hotfix.1"
        );
    }
}
