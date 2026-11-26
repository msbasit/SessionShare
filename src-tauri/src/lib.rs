use serde::{Deserialize, Serialize};
use sessionshare_core::claude_fs::{default_claude_dir, resolve_claude_desktop_sessions_dir};
use sessionshare_core::models::{
    ActivityEntry, AppConfig, ConflictView, GlobalSyncOptions, MergeConflict, PendingApproval,
    ProjectView, SessionSummary, SyncProgress, SyncReport,
};
use sessionshare_core::state_db::{default_app_data_dir, load_or_create_config};
use sessionshare_core::sync_engine::SyncEngine;
use sessionshare_core::watcher::SyncWatcher;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_autostart::ManagerExt;

#[cfg(target_os = "windows")]
fn set_window_corner_preference(window: &tauri::WebviewWindow) -> Result<(), String> {
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
    };

    let hwnd = window.hwnd().map_err(|error| error.to_string())?;
    let preference = DWMWCP_DONOTROUND;
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &preference as *const _ as *const _,
            std::mem::size_of_val(&preference) as u32,
        )
    }
    .map_err(|error| error.to_string())
}

/// Passed by the login auto-start entry so the app starts hidden in the tray.
const MINIMIZED_ARG: &str = "--minimized";
/// How often the background scheduler checks for a due periodic sync / dead watcher.
const SCHEDULER_TICK: Duration = Duration::from_secs(20);

struct Shared {
    engine: Mutex<SyncEngine>,
    /// Config snapshot readable while a sync holds the engine lock.
    config: Mutex<AppConfig>,
    watcher: Mutex<Option<SyncWatcher>>,
    watcher_error: Mutex<Option<String>>,
    suppress_watch: Arc<AtomicBool>,
    is_syncing: AtomicBool,
    sync_generation: AtomicU64,
    pending_count: AtomicUsize,
    last_report: Mutex<Option<SyncReport>>,
    progress: Mutex<Option<SyncProgress>>,
    last_sync_at: Mutex<Option<Instant>>,
}

type SharedState = Arc<Shared>;

/// Locks a mutex, recovering from poisoning so one panic can't wedge the app.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

async fn blocking<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncStatus {
    pub configured: bool,
    pub auto_sync: bool,
    pub is_syncing: bool,
    pub machine_name: String,
    pub last_report: Option<SyncReport>,
    pub sync_generation: u64,
    pub progress: Option<SyncProgress>,
    pub watcher_active: bool,
    pub watcher_error: Option<String>,
    pub pending_approvals: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub projects: Vec<ProjectView>,
    pub sessions: Vec<SessionSummary>,
    pub activities: Vec<ActivityEntry>,
    pub pending_approvals: Vec<PendingApproval>,
    pub conflicts: Vec<MergeConflict>,
}

#[derive(Clone, Serialize)]
struct SyncProgressEvent {
    generation: u64,
    progress: SyncProgress,
}

#[derive(Clone, Serialize)]
struct SyncCompletedEvent<'a> {
    generation: u64,
    report: &'a SyncReport,
}

#[derive(Clone, Serialize)]
struct SyncErrorEvent<'a> {
    generation: u64,
    error: &'a str,
}

/// Number of items waiting for the user (approvals + files to merge).
fn review_count(engine: &SyncEngine) -> usize {
    engine.pending_approvals().map(|p| p.len()).unwrap_or(0)
        + engine.conflicts().map(|c| c.len()).unwrap_or(0)
}

#[derive(Debug, Clone, Deserialize)]
pub struct SaveSettingsPayload {
    pub machine_name: String,
    pub claude_dir: String,
    #[serde(default)]
    pub claude_desktop_dir: Option<String>,
    pub sync_dir: Option<String>,
    pub auto_sync: bool,
    pub debounce_secs: u64,
    #[serde(default)]
    pub workspace_roots: Vec<String>,
    pub global_options: GlobalSyncOptions,
    // Optional so older/partial callers keep the current value.
    pub periodic_sync_mins: Option<u64>,
    pub active_session_grace_secs: Option<u64>,
    pub propagate_deletions: Option<bool>,
    pub require_hook_approval: Option<bool>,
    pub settings_excluded_keys: Option<Vec<String>>,
    pub retention_days: Option<u64>,
    pub launch_at_login: Option<bool>,
}

fn perform_sync(app: &AppHandle, shared: &SharedState) -> Result<SyncReport, String> {
    if shared
        .is_syncing
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("Sync already in progress".to_string());
    }
    let generation = shared.sync_generation.fetch_add(1, Ordering::SeqCst) + 1;

    shared.suppress_watch.store(true, Ordering::SeqCst);
    *lock(&shared.progress) = None;
    let _ = app.emit("sync-started", generation);

    let result = {
        let mut engine = lock(&shared.engine);
        let mut last_emit = Instant::now() - Duration::from_secs(1);
        let result = engine.sync_all_with_progress(|progress| {
            *lock(&shared.progress) = Some(progress.clone());
            if last_emit.elapsed() >= Duration::from_millis(100) || progress.phase == "Finishing sync" {
                if let Err(error) = app.emit("sync-progress", SyncProgressEvent {
                    generation,
                    progress: progress.clone(),
                }) {
                    eprintln!("SessionShare: could not emit sync progress: {error}");
                }
                last_emit = Instant::now();
            }
        }).map_err(|e| e.to_string());
        *lock(&shared.config) = engine.config().clone();
        shared
            .pending_count
            .store(review_count(&engine), Ordering::SeqCst);
        result
    };
    *lock(&shared.last_sync_at) = Some(Instant::now());
    if let Ok(rep) = &result {
        *lock(&shared.last_report) = Some(rep.clone());
    }
    *lock(&shared.progress) = None;
    shared.is_syncing.store(false, Ordering::SeqCst);

    match &result {
        Ok(rep) => {
            let _ = app.emit("sync-completed", SyncCompletedEvent { generation, report: rep });
        }
        Err(err) => {
            let _ = app.emit("sync-error", SyncErrorEvent { generation, error: err });
        }
    }

    // Our own writes surface as watcher events one debounce period later; keep the watcher
    // suppressed until then so a sync doesn't immediately trigger another one.
    let hold = Duration::from_secs(lock(&shared.config).debounce_secs.max(2) + 1);
    let s = Arc::clone(shared);
    thread::spawn(move || {
        thread::sleep(hold);
        if s.sync_generation.load(Ordering::SeqCst) == generation
            && !s.is_syncing.load(Ordering::SeqCst)
        {
            s.suppress_watch.store(false, Ordering::SeqCst);
        }
    });

    result
}

fn spawn_sync(app: &AppHandle, shared: &SharedState) {
    let app = app.clone();
    let shared = Arc::clone(shared);
    thread::spawn(move || {
        let _ = perform_sync(&app, &shared);
    });
}

fn restart_watcher(app: &AppHandle, shared: &SharedState) {
    let cfg = lock(&shared.config).clone();
    let mut slot = lock(&shared.watcher);
    *slot = None;
    *lock(&shared.watcher_error) = None;

    if !cfg.auto_sync {
        return;
    }
    let Some(sync_dir) = cfg.sync_dir.clone() else {
        return;
    };

    let desktop_dir = if cfg.global_options.desktop_sessions {
        resolve_claude_desktop_sessions_dir(cfg.claude_desktop_dir.as_deref())
    } else {
        None
    };

    let app_handle = app.clone();
    let s = Arc::clone(shared);
    match SyncWatcher::start(
        cfg.claude_dir,
        sync_dir,
        desktop_dir,
        cfg.debounce_secs,
        Arc::clone(&shared.suppress_watch),
        move || {
            let _ = perform_sync(&app_handle, &s);
        },
    ) {
        Ok(w) => *slot = Some(w),
        Err(e) => *lock(&shared.watcher_error) = Some(e.to_string()),
    }
}

/// Background loop: periodic fallback sync (file events from cloud drives are not always reliable)
/// and watcher recovery (e.g. a synced drive that wasn't mounted at startup).
fn start_scheduler(app: AppHandle, shared: SharedState) {
    thread::spawn(move || loop {
        thread::sleep(SCHEDULER_TICK);
        let cfg = lock(&shared.config).clone();
        if !cfg.auto_sync || cfg.sync_dir.is_none() {
            continue;
        }
        if lock(&shared.watcher).is_none() {
            restart_watcher(&app, &shared);
        }
        let interval = Duration::from_secs(cfg.periodic_sync_mins * 60);
        let due = match *lock(&shared.last_sync_at) {
            None => true,
            Some(t) => cfg.periodic_sync_mins > 0 && t.elapsed() >= interval,
        };
        if due {
            let _ = perform_sync(&app, &shared);
        }
    });
}

fn apply_autostart(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let manager = app.autolaunch();
    let current = manager.is_enabled().unwrap_or(false);
    if enabled && !current {
        manager.enable().map_err(|e| e.to_string())?;
    } else if !enabled && current {
        manager.disable().map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn show_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

#[tauri::command]
async fn get_config(state: State<'_, SharedState>) -> Result<AppConfig, String> {
    Ok(lock(&state.config).clone())
}

#[tauri::command]
async fn get_sync_status(state: State<'_, SharedState>) -> Result<SyncStatus, String> {
    let cfg = lock(&state.config).clone();
    let (generation, is_syncing, progress) = loop {
        let generation = state.sync_generation.load(Ordering::SeqCst);
        let is_syncing = state.is_syncing.load(Ordering::SeqCst);
        let progress = lock(&state.progress).clone();
        if generation == state.sync_generation.load(Ordering::SeqCst) {
            break (generation, is_syncing, progress);
        }
    };
    Ok(SyncStatus {
        configured: cfg.sync_dir.is_some(),
        auto_sync: cfg.auto_sync,
        is_syncing,
        machine_name: cfg.machine_name,
        last_report: lock(&state.last_report).clone(),
        sync_generation: generation,
        progress,
        watcher_active: lock(&state.watcher).is_some(),
        watcher_error: lock(&state.watcher_error).clone(),
        pending_approvals: state.pending_count.load(Ordering::SeqCst),
    })
}

#[tauri::command]
async fn save_settings(
    app: AppHandle,
    state: State<'_, SharedState>,
    payload: SaveSettingsPayload,
) -> Result<AppConfig, String> {
    let shared = Arc::clone(&state);
    blocking(move || {
        let updated = {
            let mut engine = lock(&shared.engine);
            let mut cfg = engine.config().clone();
            cfg.machine_name = payload.machine_name.trim().to_string();
            cfg.claude_dir = PathBuf::from(payload.claude_dir.trim());
            cfg.claude_desktop_dir = payload
                .claude_desktop_dir
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .map(PathBuf::from);
            cfg.sync_dir = payload
                .sync_dir
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .map(PathBuf::from);
            cfg.auto_sync = payload.auto_sync;
            cfg.debounce_secs = payload.debounce_secs.max(2);
            cfg.workspace_roots = payload
                .workspace_roots
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            cfg.global_options = payload.global_options;
            if let Some(v) = payload.periodic_sync_mins {
                cfg.periodic_sync_mins = v;
            }
            if let Some(v) = payload.active_session_grace_secs {
                cfg.active_session_grace_secs = v;
            }
            if let Some(v) = payload.propagate_deletions {
                cfg.propagate_deletions = v;
            }
            if let Some(v) = payload.require_hook_approval {
                cfg.require_hook_approval = v;
            }
            if let Some(v) = payload.settings_excluded_keys {
                cfg.settings_excluded_keys = v
                    .into_iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            if let Some(v) = payload.retention_days {
                cfg.retention_days = v.max(1);
            }
            if let Some(v) = payload.launch_at_login {
                cfg.launch_at_login = v;
            }
            engine
                .update_config(cfg.clone())
                .map_err(|e| e.to_string())?;
            cfg
        };
        *lock(&shared.config) = updated.clone();
        restart_watcher(&app, &shared);
        apply_autostart(&app, updated.launch_at_login).map_err(|e| {
            format!("Settings saved, but launch at login could not be updated: {}", e)
        })?;
        Ok(updated)
    })
    .await
}

#[tauri::command]
async fn get_overview(state: State<'_, SharedState>) -> Result<Overview, String> {
    let shared = Arc::clone(&state);
    blocking(move || {
        let mut engine = lock(&shared.engine);
        let (projects, sessions) = engine.overview().map_err(|e| e.to_string())?;
        let activities = engine.recent_activities(100).map_err(|e| e.to_string())?;
        let pending_approvals = engine.pending_approvals().map_err(|e| e.to_string())?;
        let conflicts = engine.conflicts().map_err(|e| e.to_string())?;
        shared
            .pending_count
            .store(pending_approvals.len() + conflicts.len(), Ordering::SeqCst);
        Ok(Overview {
            projects,
            sessions,
            activities,
            pending_approvals,
            conflicts,
        })
    })
    .await
}

#[tauri::command]
async fn get_conflict_view(
    state: State<'_, SharedState>,
    remote_key: String,
) -> Result<ConflictView, String> {
    let shared = Arc::clone(&state);
    blocking(move || {
        lock(&shared.engine)
            .conflict_view(&remote_key)
            .map_err(|e| e.to_string())
    })
    .await
}

#[tauri::command]
async fn resolve_conflict(
    app: AppHandle,
    state: State<'_, SharedState>,
    remote_key: String,
    local_hash: String,
    remote_hash: String,
    content: String,
) -> Result<(), String> {
    let shared = Arc::clone(&state);
    blocking(move || {
        {
            let engine = lock(&shared.engine);
            engine
                .resolve_conflict(&remote_key, &local_hash, &remote_hash, &content)
                .map_err(|e| e.to_string())?;
            shared
                .pending_count
                .store(review_count(&engine), Ordering::SeqCst);
        }
        let _ = app.emit("conflict-resolved", &remote_key);
        Ok(())
    })
    .await
}

#[tauri::command]
async fn list_projects(state: State<'_, SharedState>) -> Result<Vec<ProjectView>, String> {
    let shared = Arc::clone(&state);
    blocking(move || {
        lock(&shared.engine)
            .discover_projects()
            .map_err(|e| e.to_string())
    })
    .await
}

#[tauri::command]
async fn list_sessions(state: State<'_, SharedState>) -> Result<Vec<SessionSummary>, String> {
    let shared = Arc::clone(&state);
    blocking(move || lock(&shared.engine).list_sessions().map_err(|e| e.to_string())).await
}

#[tauri::command]
async fn map_project(
    state: State<'_, SharedState>,
    canonical_id: String,
    local_path: String,
) -> Result<(), String> {
    let shared = Arc::clone(&state);
    blocking(move || {
        let mut engine = lock(&shared.engine);
        engine
            .map_project_local_path(&canonical_id, &local_path)
            .map_err(|e| e.to_string())?;
        *lock(&shared.config) = engine.config().clone();
        Ok(())
    })
    .await
}

#[tauri::command]
async fn toggle_project_sync(
    state: State<'_, SharedState>,
    canonical_id: String,
    enabled: bool,
) -> Result<(), String> {
    let shared = Arc::clone(&state);
    blocking(move || {
        let mut engine = lock(&shared.engine);
        engine
            .set_project_sync_enabled(&canonical_id, enabled)
            .map_err(|e| e.to_string())?;
        *lock(&shared.config) = engine.config().clone();
        Ok(())
    })
    .await
}

#[tauri::command]
async fn run_sync_now(app: AppHandle, state: State<'_, SharedState>) -> Result<SyncReport, String> {
    let shared = Arc::clone(&state);
    blocking(move || perform_sync(&app, &shared)).await
}

#[tauri::command]
async fn get_recent_activities(state: State<'_, SharedState>) -> Result<Vec<ActivityEntry>, String> {
    let shared = Arc::clone(&state);
    blocking(move || {
        lock(&shared.engine)
            .recent_activities(100)
            .map_err(|e| e.to_string())
    })
    .await
}

#[tauri::command]
async fn list_pending_approvals(
    state: State<'_, SharedState>,
) -> Result<Vec<PendingApproval>, String> {
    let shared = Arc::clone(&state);
    blocking(move || {
        lock(&shared.engine)
            .pending_approvals()
            .map_err(|e| e.to_string())
    })
    .await
}

#[tauri::command]
async fn resolve_approval(
    app: AppHandle,
    state: State<'_, SharedState>,
    remote_key: String,
    remote_hash: String,
    approve: bool,
) -> Result<(), String> {
    let shared = Arc::clone(&state);
    blocking(move || {
        {
            let engine = lock(&shared.engine);
            engine
                .resolve_approval(&remote_key, &remote_hash, approve)
                .map_err(|e| e.to_string())?;
            shared
                .pending_count
                .store(review_count(&engine), Ordering::SeqCst);
        }
        if approve {
            spawn_sync(&app, &shared);
        }
        Ok(())
    })
    .await
}

pub fn run() {
    let app_data = default_app_data_dir();
    let cfg = load_or_create_config(&app_data, default_claude_dir())
        .expect("Failed to load or initialize SessionShare config");
    let engine = SyncEngine::new(app_data, cfg.clone()).expect("Failed to initialize SyncEngine");
    let start_minimized = std::env::args().any(|a| a == MINIMIZED_ARG);

    let shared: SharedState = Arc::new(Shared {
        engine: Mutex::new(engine),
        config: Mutex::new(cfg),
        watcher: Mutex::new(None),
        watcher_error: Mutex::new(None),
        suppress_watch: Arc::new(AtomicBool::new(false)),
        is_syncing: AtomicBool::new(false),
        sync_generation: AtomicU64::new(0),
        pending_count: AtomicUsize::new(0),
        last_report: Mutex::new(None),
        progress: Mutex::new(None),
        last_sync_at: Mutex::new(None),
    });

    tauri::Builder::default()
        // Must be registered first: a second launch just focuses the running instance
        // instead of starting another engine syncing the same folders.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .arg(MINIMIZED_ARG)
                .build(),
        )
        .plugin(tauri_plugin_dialog::init())
        .manage(Arc::clone(&shared))
        .setup(move |app| {
            #[cfg(target_os = "windows")]
            if let Some(window) = app.get_webview_window("main") {
                if let Err(error) = set_window_corner_preference(&window) {
                    eprintln!("SessionShare: could not disable system window rounding: {error}");
                }
            }

            let handle = app.handle().clone();
            restart_watcher(&handle, &shared);
            start_scheduler(handle.clone(), Arc::clone(&shared));

            let launch_at_login = lock(&shared.config).launch_at_login;
            if let Err(e) = apply_autostart(&handle, launch_at_login) {
                eprintln!("SessionShare: could not update launch at login: {}", e);
            }

            // Build Windows system tray menu
            let open_i = MenuItem::with_id(app, "open", "Open Dashboard", true, None::<&str>)?;
            let sync_i = MenuItem::with_id(app, "sync", "Sync Now", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit SessionShare", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_i, &sync_i, &quit_i])?;

            let mut tray_builder = TrayIconBuilder::new()
                .menu(&menu)
                .tooltip("SessionShare — Claude Code Sync")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show_main_window(app),
                    "sync" => {
                        let state = app.state::<SharedState>();
                        spawn_sync(app, &state);
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                });

            if let Some(icon) = app.default_window_icon() {
                tray_builder = tray_builder.icon(icon.clone());
            }

            let _ = tray_builder.build(app)?;

            // The window starts hidden (see tauri.conf.json); show it unless launched at login.
            if !start_minimized {
                show_main_window(&handle);
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            // Hide to system tray when user clicks X so background sync continues
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            get_sync_status,
            save_settings,
            get_overview,
            list_projects,
            map_project,
            toggle_project_sync,
            list_sessions,
            run_sync_now,
            get_recent_activities,
            list_pending_approvals,
            resolve_approval,
            get_conflict_view,
            resolve_conflict
        ])
        .run(tauri::generate_context!())
        .expect("error while running SessionShare");
}
