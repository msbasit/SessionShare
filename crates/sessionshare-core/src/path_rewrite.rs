use crate::claude_fs::normalize_windows_path;
use serde_json::Value;
use std::collections::BTreeMap;

pub const PROJECT_ROOT_TOKEN: &str = "${SESSIONSHARE_PROJECT_ROOT}";
pub const CLAUDE_DIR_TOKEN: &str = "${SESSIONSHARE_CLAUDE_DIR}";
pub const HOME_DIR_TOKEN: &str = "${SESSIONSHARE_HOME_DIR}";

/// Context containing the local machine's root paths so any machine-specific path
/// (project root, `.claude` directory, or `%USERPROFILE%`) can be normalized and restored cleanly.
#[derive(Debug, Clone)]
pub struct PathRewriteContext {
    pub project_root: Option<String>,
    pub claude_dir: String,
    pub home_dir: Option<String>,
}

impl PathRewriteContext {
    pub fn new(project_root: Option<&str>, claude_dir: &str, home_dir: Option<&str>) -> Self {
        Self {
            project_root: project_root.map(normalize_windows_path),
            claude_dir: normalize_windows_path(claude_dir),
            home_dir: home_dir.map(normalize_windows_path),
        }
    }

    /// Returns `(token, win_root, fwd_root)` pairs ordered from most specific (longest) to least specific.
    fn ordered_roots(&self) -> Vec<(&'static str, String, String)> {
        let mut items: Vec<(&'static str, String, String)> = Vec::new();
        if let Some(p) = &self.project_root {
            if !p.is_empty() {
                items.push((PROJECT_ROOT_TOKEN, p.clone(), p.replace('\\', "/")));
            }
        }
        if !self.claude_dir.is_empty() {
            items.push((
                CLAUDE_DIR_TOKEN,
                self.claude_dir.clone(),
                self.claude_dir.replace('\\', "/"),
            ));
        }
        if let Some(h) = &self.home_dir {
            if !h.is_empty() {
                items.push((HOME_DIR_TOKEN, h.clone(), h.replace('\\', "/")));
            }
        }
        // Longest root first so e.g. `C:\Users\Alice\.claude` matches before `C:\Users\Alice`
        items.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
        items
    }
}

/// Rewrites occurrences of project root, `.claude` dir, and home dir inside a `.jsonl` transcript
/// into portable tokens so transcripts work cleanly across PCs with different folder structures.
pub fn normalize_jsonl_with_ctx(content: &str, ctx: &PathRewriteContext) -> String {
    let roots = ctx.ordered_roots();
    let mut out = String::with_capacity(content.len());
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(mut val) => {
                rewrite_json_to_portable(&mut val, &roots);
                if let Ok(serialized) = serde_json::to_string(&val) {
                    out.push_str(&serialized);
                } else {
                    out.push_str(line);
                }
            }
            Err(_) => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// Expands portable tokens inside a synced `.jsonl` transcript to the target machine's local paths.
pub fn denormalize_jsonl_with_ctx(content: &str, ctx: &PathRewriteContext) -> String {
    let mut out = String::with_capacity(content.len());
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(mut val) => {
                rewrite_json_from_portable(&mut val, ctx);
                if let Ok(serialized) = serde_json::to_string(&val) {
                    out.push_str(&serialized);
                } else {
                    out.push_str(line);
                }
            }
            Err(_) => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// Convenience wrapper for normalizing `.jsonl` when only `project_root` is provided.
pub fn normalize_jsonl(content: &str, local_root: &str) -> String {
    let ctx = PathRewriteContext::new(Some(local_root), "", None);
    normalize_jsonl_with_ctx(content, &ctx)
}

/// Convenience wrapper for denormalizing `.jsonl` when only `target_local_root` is provided.
pub fn denormalize_jsonl(content: &str, target_local_root: &str) -> String {
    let ctx = PathRewriteContext::new(Some(target_local_root), "", None);
    denormalize_jsonl_with_ctx(content, &ctx)
}

/// Normalizes paths inside `settings.json` (`${SESSIONSHARE_CLAUDE_DIR}`, `${SESSIONSHARE_HOME_DIR}`).
pub fn normalize_settings_json(content: &str, ctx: &PathRewriteContext) -> String {
    let roots = ctx.ordered_roots();
    match serde_json::from_str::<Value>(content) {
        Ok(mut val) => {
            rewrite_json_to_portable(&mut val, &roots);
            serde_json::to_string_pretty(&val).unwrap_or_else(|_| content.to_string())
        }
        Err(_) => content.to_string(),
    }
}

/// Denormalizes portable path tokens inside `settings.json` back to this PC's local paths.
pub fn denormalize_settings_json(content: &str, ctx: &PathRewriteContext) -> String {
    match serde_json::from_str::<Value>(content) {
        Ok(mut val) => {
            rewrite_json_from_portable(&mut val, ctx);
            serde_json::to_string_pretty(&val).unwrap_or_else(|_| content.to_string())
        }
        Err(_) => content.to_string(),
    }
}

/// Normalizes paths inside a Claude Desktop `local_<uuid>.json` session record into portable tokens
/// (compact JSON, matching Claude Desktop's on-disk format).
pub fn normalize_desktop_session_json(content: &str, ctx: &PathRewriteContext) -> String {
    let roots = ctx.ordered_roots();
    match serde_json::from_str::<Value>(content) {
        Ok(mut val) => {
            rewrite_json_to_portable(&mut val, &roots);
            serde_json::to_string(&val).unwrap_or_else(|_| content.to_string())
        }
        Err(_) => content.to_string(),
    }
}

/// Denormalizes portable path tokens inside a Claude Desktop `local_<uuid>.json` back to this PC's paths.
pub fn denormalize_desktop_session_json(content: &str, ctx: &PathRewriteContext) -> String {
    match serde_json::from_str::<Value>(content) {
        Ok(mut val) => {
            rewrite_json_from_portable(&mut val, ctx);
            serde_json::to_string(&val).unwrap_or_else(|_| content.to_string())
        }
        Err(_) => content.to_string(),
    }
}

/// Extracts `(latest_timestamp_ms, completed_turns)` from a Claude Desktop `local_*.json` file
/// to resolve concurrent updates automatically in favor of the newer state.
pub fn desktop_session_freshness(content: &str) -> (i64, i64) {
    let Ok(val) = serde_json::from_str::<Value>(content) else {
        return (0, 0);
    };
    let ts = ["lastActivityAt", "lastTurnEdgeAt", "lastFocusedAt", "createdAt"]
        .iter()
        .filter_map(|k| val.get(*k).and_then(|v| v.as_i64()))
        .max()
        .unwrap_or(0);
    let turns = val
        .get("completedTurns")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    (ts, turns)
}

fn is_path_key(key: &str) -> bool {
    matches!(
        key,
        "cwd"
            | "originCwd"
            | "gitRoot"
            | "commonDir"
            | "realPath"
            | "projectPath"
            | "originalPath"
            | "file_path"
            | "filePath"
            | "path"
            | "directory"
            | "notebook_path"
            | "root"
            | "workingDirectory"
    )
}

fn rewrite_json_to_portable(val: &mut Value, roots: &[(&'static str, String, String)]) {
    match val {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if let Value::String(s) = v {
                    if is_path_key(k) {
                        *s = replace_roots_exact_or_prefix(s, roots);
                    } else {
                        // Also rewrite embedded root paths in hook commands / settings strings
                        *s = replace_embedded_roots(s, roots);
                    }
                } else {
                    rewrite_json_to_portable(v, roots);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                if let Value::String(s) = item {
                    *s = replace_embedded_roots(s, roots);
                } else {
                    rewrite_json_to_portable(item, roots);
                }
            }
        }
        _ => {}
    }
}

fn rewrite_json_from_portable(val: &mut Value, ctx: &PathRewriteContext) {
    match val {
        Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                if let Value::String(s) = v {
                    *s = expand_all_tokens(s, ctx);
                } else {
                    rewrite_json_from_portable(v, ctx);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                if let Value::String(s) = item {
                    *s = expand_all_tokens(s, ctx);
                } else {
                    rewrite_json_from_portable(item, ctx);
                }
            }
        }
        _ => {}
    }
}

fn replace_roots_exact_or_prefix(
    path_str: &str,
    roots: &[(&'static str, String, String)],
) -> String {
    for (token, win_root, fwd_root) in roots {
        let replaced = replace_path_prefix_with_token(path_str, win_root, fwd_root, token);
        if replaced != path_str {
            return replaced;
        }
    }
    path_str.to_string()
}

/// Replaces embedded occurrences of known root paths inside arbitrary strings (e.g. hook commands).
fn replace_embedded_roots(input: &str, roots: &[(&'static str, String, String)]) -> String {
    // Fast path: check prefix first
    let prefix_done = replace_roots_exact_or_prefix(input, roots);
    if prefix_done != input {
        return prefix_done;
    }

    let mut current = input.to_string();
    for (token, win_root, fwd_root) in roots {
        for needle in [win_root.as_str(), fwd_root.as_str()] {
            if needle.len() < 4 {
                continue;
            }
            current = replace_case_insensitive_path_occurrences(&current, needle, token);
        }
    }
    current
}

fn replace_case_insensitive_path_occurrences(haystack: &str, needle: &str, token: &str) -> String {
    let lower_hay = haystack.to_ascii_lowercase();
    let lower_needle = needle.to_ascii_lowercase();
    if !lower_hay.contains(&lower_needle) {
        return haystack.to_string();
    }

    let mut result = String::with_capacity(haystack.len());
    let mut pos = 0;
    while let Some(rel_idx) = lower_hay[pos..].find(&lower_needle) {
        let match_start = pos + rel_idx;
        let match_end = match_start + lower_needle.len();

        // Ensure boundary after match is end-of-string, slash, whitespace, or quote
        let next_char = haystack[match_end..].chars().next();
        let valid_boundary = matches!(
            next_char,
            None | Some('\\') | Some('/') | Some(' ') | Some('"') | Some('\'') | Some(')') | Some(',') | Some(';')
        );

        result.push_str(&haystack[pos..match_start]);
        if valid_boundary {
            result.push_str(token);
        } else {
            result.push_str(&haystack[match_start..match_end]);
        }
        pos = match_end;
    }
    result.push_str(&haystack[pos..]);
    result
}

/// Replaces `win_root` or `fwd_root` (case-insensitive on Windows) at the start of `path_str`
/// with `token` and normalizes remaining path separators to `/`.
pub fn replace_path_prefix_with_token(
    path_str: &str,
    win_root: &str,
    fwd_root: &str,
    token: &str,
) -> String {
    let lower = path_str.to_ascii_lowercase();
    let win_lower = win_root.to_ascii_lowercase();
    let fwd_lower = fwd_root.to_ascii_lowercase();

    for root_lower in [&win_lower, &fwd_lower] {
        if root_lower.is_empty() {
            continue;
        }
        if lower == *root_lower {
            return token.to_string();
        }
        if lower.starts_with(root_lower) {
            let rest = &path_str[root_lower.len()..];
            if rest.starts_with('\\') || rest.starts_with('/') {
                let posix_rest = rest.replace('\\', "/");
                return format!("{}{}", token, posix_rest);
            }
        }
    }

    path_str.to_string()
}

pub fn replace_path_prefix_to_portable(path_str: &str, win_root: &str, fwd_root: &str) -> String {
    replace_path_prefix_with_token(path_str, win_root, fwd_root, PROJECT_ROOT_TOKEN)
}

/// Expands all portable tokens (`PROJECT_ROOT_TOKEN`, `CLAUDE_DIR_TOKEN`, `HOME_DIR_TOKEN`)
/// back to local Windows paths.
pub fn expand_all_tokens(input: &str, ctx: &PathRewriteContext) -> String {
    let mut s = input.to_string();
    if let Some(proj) = &ctx.project_root {
        s = expand_single_token(&s, PROJECT_ROOT_TOKEN, proj);
    }
    if !ctx.claude_dir.is_empty() {
        s = expand_single_token(&s, CLAUDE_DIR_TOKEN, &ctx.claude_dir);
    }
    if let Some(home) = &ctx.home_dir {
        s = expand_single_token(&s, HOME_DIR_TOKEN, home);
    }
    s
}

fn expand_single_token(input: &str, token: &str, target_win_root: &str) -> String {
    if !input.contains(token) {
        return input.to_string();
    }
    if input == token {
        return target_win_root.to_string();
    }
    if let Some(rest) = input.strip_prefix(token) {
        if rest.starts_with('/') || rest.starts_with('\\') {
            return format!("{}{}", target_win_root, rest.replace('/', "\\"));
        }
    }
    // Embedded occurrence: replace token and convert immediately following `/` subpath segments if needed
    input.replace(token, target_win_root)
}

/// Normalizes `history.jsonl` by replacing any known local project path in the `"project"` field
/// with `"${PROJECT:<canonical_id>}"`.
pub fn normalize_history_jsonl(
    content: &str,
    canonical_to_local: &BTreeMap<String, String>,
) -> String {
    let mut pairs: Vec<(String, String, String)> = canonical_to_local
        .iter()
        .map(|(cid, local)| {
            let win = normalize_windows_path(local);
            let fwd = win.replace('\\', "/");
            (cid.clone(), win, fwd)
        })
        .collect();
    // Longest paths first to avoid partial prefix collisions
    pairs.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    let mut out = String::with_capacity(content.len());
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(mut val) => {
                if let Some(proj) = val
                    .get("project")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                {
                    for (cid, win, fwd) in &pairs {
                        let replaced = replace_path_prefix_to_portable(&proj, win, fwd);
                        if replaced != proj {
                            let token = replaced.replace(
                                PROJECT_ROOT_TOKEN,
                                &format!("${{PROJECT:{}}}", cid),
                            );
                            val["project"] = Value::String(token);
                            break;
                        }
                    }
                }
                if let Ok(s) = serde_json::to_string(&val) {
                    out.push_str(&s);
                } else {
                    out.push_str(line);
                }
            }
            Err(_) => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// Expands `"${PROJECT:<canonical_id>}"` tokens in `history.jsonl` back to this machine's local paths.
pub fn denormalize_history_jsonl(
    content: &str,
    canonical_to_local: &BTreeMap<String, String>,
) -> String {
    let mut out = String::with_capacity(content.len());
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(mut val) => {
                if let Some(proj) = val
                    .get("project")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                {
                    if let Some(rest) = proj.strip_prefix("${PROJECT:") {
                        if let Some(end_idx) = rest.find('}') {
                            let cid = &rest[..end_idx];
                            let suffix = &rest[end_idx + 1..];
                            if let Some(local_root) = canonical_to_local.get(cid) {
                                let target = normalize_windows_path(local_root);
                                let win_suffix = suffix.replace('/', "\\");
                                val["project"] = Value::String(format!("{}{}", target, win_suffix));
                            }
                        }
                    }
                }
                if let Ok(s) = serde_json::to_string(&val) {
                    out.push_str(&s);
                } else {
                    out.push_str(line);
                }
            }
            Err(_) => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_windows_paths_between_machines() {
        let machine_a_jsonl = r#"{"type":"user","uuid":"u1","cwd":"C:\\Users\\Personal\\Dev\\App","message":{"content":"hi"}}
{"type":"assistant","uuid":"u2","cwd":"C:\\Users\\Personal\\Dev\\App\\src","toolUse":{"file_path":"C:\\Users\\Personal\\Dev\\App\\src\\main.rs"}}
"#;
        let ctx_a = PathRewriteContext::new(
            Some(r"C:\Users\Personal\Dev\App"),
            r"C:\Users\Personal\.claude",
            Some(r"C:\Users\Personal"),
        );
        let ctx_b = PathRewriteContext::new(
            Some(r"D:\Work\Repos\App"),
            r"C:\Users\WorkUser\.claude",
            Some(r"C:\Users\WorkUser"),
        );

        let portable = normalize_jsonl_with_ctx(machine_a_jsonl, &ctx_a);
        assert!(portable.contains(PROJECT_ROOT_TOKEN));
        assert!(portable.contains("${SESSIONSHARE_PROJECT_ROOT}/src/main.rs"));

        let machine_b = denormalize_jsonl_with_ctx(&portable, &ctx_b);
        assert!(machine_b.contains(r#"D:\\Work\\Repos\\App"#));
        assert!(machine_b.contains(r#"D:\\Work\\Repos\\App\\src\\main.rs"#));
    }

    #[test]
    fn round_trips_settings_json_claude_and_home_paths() {
        let settings = r#"{
  "hooks": {
    "PreToolUse": [
      { "command": "pwsh -File C:\\Users\\Personal\\.claude\\hooks\\lint.ps1" }
    ]
  },
  "additionalDirectories": ["C:\\Users\\Personal\\notes"]
}"#;
        let ctx_a = PathRewriteContext::new(
            None,
            r"C:\Users\Personal\.claude",
            Some(r"C:\Users\Personal"),
        );
        let ctx_b = PathRewriteContext::new(
            None,
            r"D:\CustomClaudeConfig",
            Some(r"C:\Users\OfficeUser"),
        );

        let portable = normalize_settings_json(settings, &ctx_a);
        assert!(portable.contains(CLAUDE_DIR_TOKEN));
        assert!(portable.contains(HOME_DIR_TOKEN));
        assert!(!portable.contains("Personal"));

        let restored = denormalize_settings_json(&portable, &ctx_b);
        assert!(restored.contains(r"D:\\CustomClaudeConfig\\hooks\\lint.ps1"));
        assert!(restored.contains(r"C:\\Users\\OfficeUser\\notes"));
    }

    #[test]
    fn round_trips_history_jsonl_project_paths() {
        let mut map_a = BTreeMap::new();
        map_a.insert("git-abc123".to_string(), r"C:\Dev\Personal\App".to_string());
        let mut map_b = BTreeMap::new();
        map_b.insert("git-abc123".to_string(), r"D:\Office\App".to_string());

        let hist = r#"{"display":"fix bug","timestamp":1700000000,"project":"C:\\Dev\\Personal\\App","sessionId":"s1"}"#;
        let portable = normalize_history_jsonl(hist, &map_a);
        assert!(portable.contains("${PROJECT:git-abc123}"));

        let restored = denormalize_history_jsonl(&portable, &map_b);
        assert!(restored.contains(r#"D:\\Office\\App"#));
    }

    #[test]
    fn round_trips_desktop_session_json_paths() {
        let session = r#"{"sessionId":"local_1","cliSessionId":"cli-1","cwd":"C:\\Users\\Personal\\Dev\\App","originCwd":"C:\\Users\\Personal\\Dev\\App","gitAnchors":[{"gitRoot":"C:\\Users\\Personal\\Dev\\App","commonDir":"C:\\Users\\Personal\\Dev\\App\\.git"}],"folderPlace":{"cwd":"C:\\Users\\Personal\\Dev\\App","realPath":"C:\\Users\\Personal\\Dev\\App"},"shellFolders":["C:\\Users\\Personal\\Dev\\App\\src\\ui"],"lastActivityAt":2000,"completedTurns":3}"#;
        let ctx_a = PathRewriteContext::new(
            Some(r"C:\Users\Personal\Dev\App"),
            r"C:\Users\Personal\.claude",
            Some(r"C:\Users\Personal"),
        );
        let ctx_b = PathRewriteContext::new(
            Some(r"D:\Work\Repos\App"),
            r"C:\Users\Work\.claude",
            Some(r"C:\Users\Work"),
        );

        let portable = normalize_desktop_session_json(session, &ctx_a);
        assert!(portable.contains(PROJECT_ROOT_TOKEN));
        assert!(!portable.contains("Personal"));
        assert_eq!(desktop_session_freshness(&portable), (2000, 3));

        let restored = denormalize_desktop_session_json(&portable, &ctx_b);
        let parsed: Value = serde_json::from_str(&restored).unwrap();
        assert_eq!(parsed["cwd"], r"D:\Work\Repos\App");
        assert_eq!(parsed["originCwd"], r"D:\Work\Repos\App");
        assert_eq!(parsed["gitAnchors"][0]["gitRoot"], r"D:\Work\Repos\App");
        assert_eq!(parsed["gitAnchors"][0]["commonDir"], r"D:\Work\Repos\App\.git");
        assert_eq!(parsed["folderPlace"]["realPath"], r"D:\Work\Repos\App");
        assert_eq!(parsed["shellFolders"][0], r"D:\Work\Repos\App\src\ui");
    }
}
