use crate::error::{Result, SessionShareError};
use crate::storage::{REMOTE_TRASH_DIR, TMP_SUFFIX};
use notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebouncedEventKind, Debouncer};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Top-level entries under `~/.claude` that SessionShare syncs.
const CLAUDE_SYNCED_DIRS: [&str; 7] = [
    "projects", "skills", "agents", "commands", "plans", "rules", "hooks",
];
const CLAUDE_SYNCED_FILES: [&str; 3] = ["CLAUDE.md", "settings.json", "history.jsonl"];
/// Top-level entries in the synced drive folder that carry content from other PCs.
/// (`machines/` is deliberately excluded: manifests change on every PC and would cause sync loops.)
const REMOTE_SYNCED_DIRS: [&str; 2] = ["projects", "global"];
const REMOTE_SYNCED_FILES: [&str; 1] = ["registry.json"];

fn first_component_and_rest(path: &Path, root: &Path) -> Option<(String, bool)> {
    let rel = path.strip_prefix(root).ok()?;
    let mut comps = rel.components();
    let first = comps.next()?.as_os_str().to_string_lossy().to_string();
    let has_more = comps.next().is_some();
    Some((first, has_more))
}

/// Returns true if a filesystem event at `path` could affect what SessionShare syncs.
pub fn is_relevant_path(path: &Path, claude_dir: &Path, sync_dir: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.ends_with(TMP_SUFFIX)
        || name.ends_with(".tmp")
        || name.contains(".conflict.")
        || name == "sessions-index.json"
    {
        return false;
    }

    // Check the synced folder first: it may be nested inside the Claude folder or vice versa.
    if let Some((first, has_more)) = first_component_and_rest(path, sync_dir) {
        if first == REMOTE_TRASH_DIR {
            return false;
        }
        return if has_more {
            REMOTE_SYNCED_DIRS.contains(&first.as_str())
        } else {
            REMOTE_SYNCED_FILES.contains(&first.as_str())
                || REMOTE_SYNCED_DIRS.contains(&first.as_str())
        };
    }
    if let Some((first, has_more)) = first_component_and_rest(path, claude_dir) {
        return if has_more {
            CLAUDE_SYNCED_DIRS.contains(&first.as_str())
        } else {
            CLAUDE_SYNCED_FILES.contains(&first.as_str())
                || CLAUDE_SYNCED_DIRS.contains(&first.as_str())
        };
    }
    false
}

/// Watches both `~/.claude` and the synced drive folder with debouncing,
/// ignoring unrelated/temporary files and suppressing self-triggered events during active syncs.
pub struct SyncWatcher {
    _debouncer: Debouncer<notify::RecommendedWatcher>,
}

impl SyncWatcher {
    pub fn start<F>(
        claude_dir: PathBuf,
        sync_dir: PathBuf,
        debounce_secs: u64,
        suppress_flag: Arc<AtomicBool>,
        on_change: F,
    ) -> Result<Self>
    where
        F: Fn() + Send + Sync + 'static,
    {
        // Create both roots so they can be watched even before Claude Code / the drive created them.
        fs::create_dir_all(&claude_dir)?;
        fs::create_dir_all(&sync_dir)?;

        let cb = Arc::new(on_change);
        let claude_root = claude_dir.clone();
        let sync_root = sync_dir.clone();
        let mut debouncer = new_debouncer(
            Duration::from_secs(debounce_secs.max(2)),
            move |res: std::result::Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| {
                if suppress_flag.load(Ordering::SeqCst) {
                    return;
                }
                if let Ok(events) = res {
                    let relevant = events.iter().any(|ev| {
                        ev.kind == DebouncedEventKind::Any
                            && is_relevant_path(&ev.path, &claude_root, &sync_root)
                    });
                    if relevant {
                        cb();
                    }
                }
            },
        )
        .map_err(|e| SessionShareError::Watch(e.to_string()))?;

        debouncer
            .watcher()
            .watch(&claude_dir, RecursiveMode::Recursive)
            .map_err(|e| SessionShareError::Watch(format!("{}: {}", claude_dir.display(), e)))?;
        debouncer
            .watcher()
            .watch(&sync_dir, RecursiveMode::Recursive)
            .map_err(|e| SessionShareError::Watch(format!("{}: {}", sync_dir.display(), e)))?;

        Ok(Self {
            _debouncer: debouncer,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_out_unsynced_and_self_generated_paths() {
        let claude = Path::new(r"C:\Users\A\.claude");
        let sync = Path::new(r"C:\Users\A\OneDrive\SessionShare");
        let rel = |p: &str| is_relevant_path(Path::new(p), claude, sync);

        assert!(rel(r"C:\Users\A\.claude\projects\C--Dev-App\s1.jsonl"));
        assert!(rel(r"C:\Users\A\.claude\settings.json"));
        assert!(rel(r"C:\Users\A\.claude\skills\x\SKILL.md"));
        assert!(rel(r"C:\Users\A\OneDrive\SessionShare\global\CLAUDE.md"));
        assert!(rel(r"C:\Users\A\OneDrive\SessionShare\registry.json"));

        assert!(!rel(r"C:\Users\A\.claude\todos\abc.json"));
        assert!(!rel(r"C:\Users\A\.claude\shell-snapshots\snap.sh"));
        assert!(!rel(r"C:\Users\A\.claude\projects\C--Dev-App\sessions-index.json"));
        assert!(!rel(r"C:\Users\A\OneDrive\SessionShare\machines\m1.json"));
        assert!(!rel(r"C:\Users\A\OneDrive\SessionShare\.trash\20260101-000000\global\a.md"));
        assert!(!rel(r"C:\Users\A\.claude\settings.json.conflict.20260101"));
    }
}
