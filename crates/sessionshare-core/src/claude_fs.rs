use crate::error::Result;
use crate::models::CanonicalProject;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Returns the user's home directory (`%USERPROFILE%`) normalized for Windows.
pub fn default_home_dir() -> Option<String> {
    dirs::home_dir().map(|p| normalize_windows_path(&p.to_string_lossy()))
}

/// Computes a portable home-relative hint (e.g. `Dev/MyApp`) if `local_path` lives under `home_dir`.
pub fn compute_home_relative_hint(local_path: &str, home_dir: Option<&str>) -> Option<String> {
    let home = normalize_windows_path(home_dir?);
    let norm = normalize_windows_path(local_path);
    let home_lower = home.to_ascii_lowercase();
    let norm_lower = norm.to_ascii_lowercase();
    if norm_lower.starts_with(&home_lower) {
        let rest = &norm[home.len()..];
        if let Some(stripped) = rest.strip_prefix('\\') {
            if !stripped.is_empty() {
                return Some(stripped.replace('\\', "/"));
            }
        }
    }
    None
}

/// Reuses an existing case-insensitive encoded project folder under `~/.claude/projects/`
/// if one already exists on Windows, preventing duplicate `c--dev-app` vs `C--Dev-App` folders.
pub fn resolve_encoded_project_dir(claude_dir: &Path, local_root: &str) -> PathBuf {
    let projects_root = claude_dir.join("projects");
    let expected = encode_project_path(local_root);
    let expected_lower = expected.to_ascii_lowercase();

    if projects_root.is_dir() {
        if let Ok(entries) = fs::read_dir(&projects_root) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.to_ascii_lowercase() == expected_lower {
                        return entry.path();
                    }
                }
            }
        }
    }

    projects_root.join(expected)
}

/// Smartly tries to locate a matching local directory on disk for an unmapped remote project:
/// 1. Same relative path under `%USERPROFILE%` (handles different Windows usernames)
/// 2. Matching `.git/config` remote URL inside workspace search roots (depth 1–2)
/// 3. Matching folder name inside workspace search roots (when Git remote doesn't conflict)
pub fn auto_resolve_unmapped_project(
    proj: &CanonicalProject,
    home_dir: Option<&str>,
    search_roots: &[PathBuf],
) -> Option<(String, &'static str)> {
    // Tier 1: Home-relative path (e.g. C:\Users\Personal\Dev\App -> C:\Users\Work\Dev\App)
    if let (Some(home), Some(hint)) = (home_dir, proj.home_relative_hint.as_deref()) {
        let candidate = PathBuf::from(home).join(hint.replace('/', "\\"));
        if candidate.is_dir() {
            let cand_git = detect_git_remote(&candidate);
            let git_ok = match (&proj.git_remote_normalized, &cand_git) {
                (Some(expected), Some(actual)) => expected == actual,
                _ => true,
            };
            if git_ok {
                return Some((
                    normalize_windows_path(&candidate.to_string_lossy()),
                    "home-relative",
                ));
            }
        }
    }

    // Collect candidate directories up to depth 2 under search_roots
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    for root in search_roots {
        if !root.is_dir() {
            continue;
        }
        collect_workspace_dirs(root, 2, &mut visited, &mut candidates);
    }

    // Tier 2: Exact Git remote match anywhere under workspace search roots
    if let Some(expected_remote) = &proj.git_remote_normalized {
        for cand in &candidates {
            if let Some(actual_remote) = detect_git_remote(cand) {
                if &actual_remote == expected_remote {
                    return Some((normalize_windows_path(&cand.to_string_lossy()), "git"));
                }
            }
        }
    }

    // Tier 3: Matching folder name under workspace search roots (no conflicting Git remote)
    let target_name_lower = proj.display_name.to_ascii_lowercase();
    for cand in &candidates {
        let Some(name) = cand.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.to_ascii_lowercase() != target_name_lower {
            continue;
        }
        let cand_git = detect_git_remote(cand);
        let git_conflict = match (&proj.git_remote_normalized, &cand_git) {
            (Some(expected), Some(actual)) => expected != actual,
            _ => false,
        };
        if !git_conflict {
            return Some((
                normalize_windows_path(&cand.to_string_lossy()),
                "workspace-scan",
            ));
        }
    }

    None
}

fn collect_workspace_dirs(
    dir: &Path,
    depth: usize,
    visited: &mut BTreeSet<String>,
    out: &mut Vec<PathBuf>,
) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.')
            || matches!(
                name.to_ascii_lowercase().as_str(),
                "node_modules" | "target" | "dist" | "build" | "venv" | ".venv" | "__pycache__"
            )
        {
            continue;
        }
        let path = entry.path();
        let key = normalize_windows_path(&path.to_string_lossy()).to_ascii_lowercase();
        if visited.insert(key) {
            out.push(path.clone());
            // Don't recurse inside a git repo when scanning for sibling repos
            if !path.join(".git").exists() && depth > 1 {
                collect_workspace_dirs(&path, depth - 1, visited, out);
            }
        }
    }
}

/// Returns the local Claude Code config directory (`CLAUDE_CONFIG_DIR` or `%USERPROFILE%\.claude`).
pub fn default_claude_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !custom.trim().is_empty() {
            return PathBuf::from(custom);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
}

/// Encodes an absolute local path into Claude Code's project folder name.
/// Every non-ASCII-alphanumeric character (including `:`, `\`, `/`, spaces, `.`, `_`)
/// is replaced with `-`.
/// Example: `C:\Users\Alice\Projects\my-app` -> `C--Users-Alice-Projects-my-app`
pub fn encode_project_path(path: &str) -> String {
    let trimmed = path.trim().trim_end_matches(['\\', '/']);
    trimmed
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Normalizes a Windows path for consistent comparison (trims trailing slashes, normalizes separators).
pub fn normalize_windows_path(path: &str) -> String {
    let mut s = path.trim().replace('/', "\\");
    while s.ends_with('\\') && s.len() > 3 {
        s.pop();
    }
    // Uppercase Windows drive letter if present (e.g., c:\ -> C:\)
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_lowercase() {
        let upper = (bytes[0] as char).to_ascii_uppercase();
        s.replace_range(0..1, &upper.to_string());
    }
    s
}

/// Discovered local project under `~/.claude/projects/<encoded-name>`.
#[derive(Debug, Clone)]
pub struct LocalClaudeProject {
    pub encoded_name: String,
    pub claude_project_dir: PathBuf,
    pub detected_local_path: Option<String>,
    pub git_remote_normalized: Option<String>,
    pub session_files: Vec<PathBuf>,
}

/// Scans `~/.claude/projects/` and detects each project's true `cwd` and Git remote (if any).
pub fn scan_local_projects(claude_dir: &Path) -> Result<Vec<LocalClaudeProject>> {
    let projects_root = claude_dir.join("projects");
    if !projects_root.exists() {
        return Ok(Vec::new());
    }

    let mut results = Vec::new();
    for entry in fs::read_dir(&projects_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let encoded_name = entry.file_name().to_string_lossy().to_string();
        let dir_path = entry.path();

        let mut session_files = Vec::new();
        for sub in fs::read_dir(&dir_path)? {
            let sub = sub?;
            let p = sub.path();
            if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                session_files.push(p);
            }
        }

        let detected_local_path = detect_project_cwd(&dir_path, &session_files)
            .map(|p| normalize_windows_path(&p));
        let git_remote_normalized = detected_local_path
            .as_deref()
            .and_then(|p| detect_git_remote(Path::new(p)));

        results.push(LocalClaudeProject {
            encoded_name,
            claude_project_dir: dir_path,
            detected_local_path,
            git_remote_normalized,
            session_files,
        });
    }

    Ok(results)
}

/// Inspects `sessions-index.json` or `.jsonl` lines inside a Claude project folder to find the real `cwd`.
pub fn detect_project_cwd(project_dir: &Path, session_files: &[PathBuf]) -> Option<String> {
    // 1. Try sessions-index.json first
    let index_path = project_dir.join("sessions-index.json");
    if index_path.is_file() {
        if let Ok(raw) = fs::read_to_string(&index_path) {
            if let Ok(val) = serde_json::from_str::<Value>(&raw) {
                if let Some(p) = val
                    .get("originalPath")
                    .or_else(|| val.get("projectPath"))
                    .and_then(|v| v.as_str())
                {
                    if !p.trim().is_empty() {
                        return Some(p.to_string());
                    }
                }
                if let Some(entries) = val.get("entries").and_then(|v| v.as_array()) {
                    for item in entries {
                        if let Some(p) = item.get("projectPath").and_then(|v| v.as_str()) {
                            if !p.trim().is_empty() {
                                return Some(p.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Scan first few lines of .jsonl files for top-level `cwd`
    for file_path in session_files {
        if let Ok(file) = File::open(file_path) {
            let reader = BufReader::new(file);
            for line in reader.lines().take(30).flatten() {
                if let Ok(val) = serde_json::from_str::<Value>(&line) {
                    if let Some(cwd) = val.get("cwd").and_then(|v| v.as_str()) {
                        if !cwd.trim().is_empty() {
                            return Some(cwd.to_string());
                        }
                    }
                }
            }
        }
    }

    None
}

/// Reads `.git/config` directly (without spawning git.exe) to find `origin` remote URL and normalize it.
pub fn detect_git_remote(repo_dir: &Path) -> Option<String> {
    let git_dir = repo_dir.join(".git");
    let config_path = if git_dir.is_dir() {
        git_dir.join("config")
    } else if git_dir.is_file() {
        // Handle git worktrees / submodules: `gitdir: <path>`
        let content = fs::read_to_string(&git_dir).ok()?;
        let line = content.lines().find(|l| l.trim().starts_with("gitdir:"))?;
        let raw_path = line.trim().trim_start_matches("gitdir:").trim();
        let resolved = if Path::new(raw_path).is_absolute() {
            PathBuf::from(raw_path)
        } else {
            repo_dir.join(raw_path)
        };
        resolved.join("config")
    } else {
        return None;
    };

    let content = fs::read_to_string(config_path).ok()?;
    let mut in_origin = false;
    let mut first_remote_url: Option<String> = None;

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_origin = line.eq_ignore_ascii_case("[remote \"origin\"]");
            continue;
        }
        if let Some(rest) = line.strip_prefix("url") {
            if let Some(val) = rest.trim_start().strip_prefix('=') {
                let url = val.trim();
                if in_origin {
                    return Some(normalize_git_url(url));
                }
                if first_remote_url.is_none() {
                    first_remote_url = Some(normalize_git_url(url));
                }
            }
        }
    }

    first_remote_url
}

/// Normalizes SSH and HTTPS Git remote URLs to a canonical lowercase form (`host/org/repo`).
pub fn normalize_git_url(url: &str) -> String {
    let mut s = url.trim().to_string();
    if let Some(stripped) = s.strip_prefix("https://") {
        s = stripped.to_string();
    } else if let Some(stripped) = s.strip_prefix("http://") {
        s = stripped.to_string();
    } else if let Some(stripped) = s.strip_prefix("ssh://") {
        s = stripped.to_string();
    }
    // Strip user@ prefix (e.g., git@github.com:org/repo)
    if let Some(at_pos) = s.find('@') {
        if s[..at_pos].chars().all(|c| c != '/') {
            s = s[at_pos + 1..].to_string();
        }
    }
    // Convert SCP-style `host:org/repo` to `host/org/repo`
    if let Some(colon_pos) = s.find(':') {
        let after = &s[colon_pos + 1..];
        if !after.starts_with('/') {
            s.replace_range(colon_pos..colon_pos + 1, "/");
        }
    }
    s = s.trim_end_matches('/').to_string();
    if let Some(stripped) = s.strip_suffix(".git") {
        s = stripped.to_string();
    }
    s.to_ascii_lowercase()
}

/// Deterministically derives a canonical project ID from a normalized Git remote or fallback name.
pub fn canonical_id_from_git(normalized_remote: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalized_remote.as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!("git-{}", &digest[..12])
}

pub fn canonical_id_from_name(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    let digest = hex::encode(hasher.finalize());
    if slug.is_empty() {
        format!("proj-{}", &digest[..10])
    } else {
        let short = &slug[..slug.len().min(20)];
        format!("proj-{}-{}", short, &digest[..6])
    }
}

pub fn display_name_from_path(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

#[derive(Debug, Clone)]
pub struct SessionFileMeta {
    pub title: String,
    pub last_modified: Option<DateTime<Utc>>,
    pub line_count: usize,
    pub size_bytes: u64,
}

/// Extracts a human-readable title, line count, size, and last modified time from a `.jsonl` transcript.
pub fn inspect_session_file(path: &Path) -> SessionFileMeta {
    let meta = fs::metadata(path).ok();
    let size_bytes = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let mut last_modified: Option<DateTime<Utc>> = meta
        .and_then(|m| m.modified().ok())
        .map(DateTime::<Utc>::from);

    let mut title = String::new();
    let mut line_count = 0usize;

    if let Ok(file) = File::open(path) {
        let reader = BufReader::new(file);
        for line in reader.lines().flatten() {
            if line.trim().is_empty() {
                continue;
            }
            line_count += 1;
            if let Ok(val) = serde_json::from_str::<Value>(&line) {
                if let Some(ts_str) = val.get("timestamp").and_then(|v| v.as_str()) {
                    if let Ok(parsed) = DateTime::parse_from_rfc3339(ts_str) {
                        let utc = parsed.with_timezone(&Utc);
                        if last_modified.map(|lm| utc > lm).unwrap_or(true) {
                            last_modified = Some(utc);
                        }
                    }
                }
                if title.is_empty() {
                    if let Some(t) = extract_user_prompt(&val) {
                        title = t;
                    }
                }
            }
        }
    }

    if title.is_empty() {
        title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Untitled Session")
            .to_string();
    }

    SessionFileMeta {
        title,
        last_modified,
        line_count,
        size_bytes,
    }
}

fn extract_user_prompt(val: &Value) -> Option<String> {
    let msg_type = val.get("type").and_then(|v| v.as_str())?;
    if msg_type != "user" {
        return None;
    }
    if val.get("isMeta").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }
    let content = val.get("message").and_then(|m| m.get("content"))?;
    let raw = if let Some(s) = content.as_str() {
        s.to_string()
    } else if let Some(arr) = content.as_array() {
        arr.iter()
            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        return None;
    };

    let cleaned = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() || cleaned.starts_with("<command-") || cleaned.starts_with("<local-command") {
        return None;
    }
    let truncated: String = cleaned.chars().take(90).collect();
    Some(truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_windows_paths_like_claude_code() {
        assert_eq!(
            encode_project_path(r"C:\Users\Alice\Projects\my-app"),
            "C--Users-Alice-Projects-my-app"
        );
        assert_eq!(
            encode_project_path(r"D:\Dev\Personal\SessionShare\"),
            "D--Dev-Personal-SessionShare"
        );
    }

    #[test]
    fn normalizes_ssh_and_https_git_urls_identically() {
        assert_eq!(
            normalize_git_url("git@github.com:Alice/SessionShare.git"),
            "github.com/alice/sessionshare"
        );
        assert_eq!(
            normalize_git_url("https://github.com/Alice/SessionShare.git"),
            "github.com/alice/sessionshare"
        );
        assert_eq!(
            normalize_git_url("https://github.com/alice/sessionshare/"),
            "github.com/alice/sessionshare"
        );
    }
}
