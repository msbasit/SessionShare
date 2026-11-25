use crate::error::{Result, SessionShareError};
use crate::storage::TMP_SUFFIX;
use notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebouncedEventKind, Debouncer};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Watches both `~/.claude` and the synced drive folder with debouncing,
/// ignoring temporary files and suppressing self-triggered events during active syncs.
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
        let cb = Arc::new(on_change);
        let mut debouncer = new_debouncer(
            Duration::from_secs(debounce_secs.max(2)),
            move |res: std::result::Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| {
                if suppress_flag.load(Ordering::SeqCst) {
                    return;
                }
                if let Ok(events) = res {
                    let relevant = events.iter().any(|ev| {
                        if ev.kind != DebouncedEventKind::Any {
                            return false;
                        }
                        let name = ev
                            .path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("");
                        !name.ends_with(TMP_SUFFIX)
                            && !name.ends_with(".tmp")
                            && name != "sessions-index.json"
                    });
                    if relevant {
                        cb();
                    }
                }
            },
        )
        .map_err(|e| SessionShareError::Watch(e.to_string()))?;

        if claude_dir.exists() {
            debouncer
                .watcher()
                .watch(&claude_dir, RecursiveMode::Recursive)
                .map_err(|e| SessionShareError::Watch(e.to_string()))?;
        }
        if sync_dir.exists() {
            debouncer
                .watcher()
                .watch(&sync_dir, RecursiveMode::Recursive)
                .map_err(|e| SessionShareError::Watch(e.to_string()))?;
        }

        Ok(Self {
            _debouncer: debouncer,
        })
    }
}
