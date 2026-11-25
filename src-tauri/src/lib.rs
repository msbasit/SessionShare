use serde::{Deserialize, Serialize};
use sessionshare_core::claude_fs::default_claude_dir;
use sessionshare_core::models::{
    ActivityEntry, AppConfig, GlobalSyncOptions, ProjectView, SessionSummary, SyncReport,
};
use sessionshare_core::state_db::{default_app_data_dir, load_or_create_config};
use sessionshare_core::sync_engine::SyncEngine;
use sessionshare_core::watcher::SyncWatcher;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};

struct AppState {
    engine: Arc<Mutex<SyncEngine>>,
    watcher: Mutex<Option<SyncWatcher>>,
    suppress_watch: Arc<AtomicBool>,
    is_syncing: Arc<AtomicBool>,
    last_report: Arc<Mutex<Option<SyncReport>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncStatus {
    pub configured: bool,
    pub auto_sync: bool,
    pub is_syncing: bool,
    pub machine_name: String,
    pub last_report: Option<SyncReport>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SaveSettingsPayload {
    pub machine_name: String,
    pub claude_dir: String,
    pub sync_dir: Option<String>,
    pub auto_sync: bool,
    pub debounce_secs: u64,
    #[serde(default)]
    pub workspace_roots: Vec<String>,
    pub global_options: GlobalSyncOptions,
}

fn perform_sync(
    app: &AppHandle,
    engine: &Arc<Mutex<SyncEngine>>,
    suppress_watch: &Arc<AtomicBool>,
    is_syncing: &Arc<AtomicBool>,
    last_report: &Arc<Mutex<Option<SyncReport>>>,
) -> Result<SyncReport, String> {
    if is_syncing
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("Sync already in progress".to_string());
    }

    suppress_watch.store(true, Ordering::SeqCst);
    let _ = app.emit("sync-started", ());

    let result = {
        let mut guard = engine.lock().map_err(|e| e.to_string())?;
        guard.sync_all().map_err(|e| e.to_string())
    };

    if let Ok(ref rep) = result {
        if let Ok(mut lr) = last_report.lock() {
            *lr = Some(rep.clone());
        }
        let _ = app.emit("sync-completed", rep);
    } else if let Err(ref err) = result {
        let _ = app.emit("sync-error", err);
    }

    is_syncing.store(false, Ordering::SeqCst);

    // Keep watcher suppressed briefly so our own writes don't immediately re-trigger a sync
    let suppress_clone = Arc::clone(suppress_watch);
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(2));
        suppress_clone.store(false, Ordering::SeqCst);
    });

    result
}

fn restart_watcher(app: &AppHandle, state: &AppState) {
    let cfg = {
        let Ok(guard) = state.engine.lock() else {
            return;
        };
        guard.config().clone()
    };

    let Ok(mut watcher_slot) = state.watcher.lock() else {
        return;
    };
    *watcher_slot = None;

    if !cfg.auto_sync {
        return;
    }
    let Some(sync_dir) = cfg.sync_dir.clone() else {
        return;
    };

    let app_handle = app.clone();
    let engine = Arc::clone(&state.engine);
    let suppress = Arc::clone(&state.suppress_watch);
    let is_syncing = Arc::clone(&state.is_syncing);
    let last_report = Arc::clone(&state.last_report);

    let suppress_for_cb = Arc::clone(&suppress);
    if let Ok(w) = SyncWatcher::start(
        cfg.claude_dir,
        sync_dir,
        cfg.debounce_secs,
        suppress,
        move || {
            let _ = perform_sync(
                &app_handle,
                &engine,
                &suppress_for_cb,
                &is_syncing,
                &last_report,
            );
        },
    ) {
        *watcher_slot = Some(w);
    }
}

#[tauri::command]
fn get_config(state: State<'_, AppState>) -> Result<AppConfig, String> {
    let guard = state.engine.lock().map_err(|e| e.to_string())?;
    Ok(guard.config().clone())
}

#[tauri::command]
fn get_sync_status(state: State<'_, AppState>) -> Result<SyncStatus, String> {
    let cfg = {
        let guard = state.engine.lock().map_err(|e| e.to_string())?;
        guard.config().clone()
    };
    let last_report = state
        .last_report
        .lock()
        .map_err(|e| e.to_string())?
        .clone();
    Ok(SyncStatus {
        configured: cfg.sync_dir.is_some(),
        auto_sync: cfg.auto_sync,
        is_syncing: state.is_syncing.load(Ordering::SeqCst),
        machine_name: cfg.machine_name,
        last_report,
    })
}

#[tauri::command]
fn save_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    payload: SaveSettingsPayload,
) -> Result<AppConfig, String> {
    let updated = {
        let mut guard = state.engine.lock().map_err(|e| e.to_string())?;
        let mut cfg = guard.config().clone();
        cfg.machine_name = payload.machine_name.trim().to_string();
        cfg.claude_dir = PathBuf::from(payload.claude_dir.trim());
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
        guard.update_config(cfg.clone()).map_err(|e| e.to_string())?;
        cfg
    };

    restart_watcher(&app, &state);
    Ok(updated)
}

#[tauri::command]
fn list_projects(state: State<'_, AppState>) -> Result<Vec<ProjectView>, String> {
    let mut guard = state.engine.lock().map_err(|e| e.to_string())?;
    guard.discover_projects().map_err(|e| e.to_string())
}

#[tauri::command]
fn map_project(
    state: State<'_, AppState>,
    canonical_id: String,
    local_path: String,
) -> Result<(), String> {
    let mut guard = state.engine.lock().map_err(|e| e.to_string())?;
    guard
        .map_project_local_path(&canonical_id, &local_path)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn toggle_project_sync(
    state: State<'_, AppState>,
    canonical_id: String,
    enabled: bool,
) -> Result<(), String> {
    let mut guard = state.engine.lock().map_err(|e| e.to_string())?;
    guard
        .set_project_sync_enabled(&canonical_id, enabled)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_sessions(state: State<'_, AppState>) -> Result<Vec<SessionSummary>, String> {
    let mut guard = state.engine.lock().map_err(|e| e.to_string())?;
    guard.list_sessions().map_err(|e| e.to_string())
}

#[tauri::command]
fn run_sync_now(app: AppHandle, state: State<'_, AppState>) -> Result<SyncReport, String> {
    perform_sync(
        &app,
        &state.engine,
        &state.suppress_watch,
        &state.is_syncing,
        &state.last_report,
    )
}

#[tauri::command]
fn get_recent_activities(state: State<'_, AppState>) -> Result<Vec<ActivityEntry>, String> {
    let guard = state.engine.lock().map_err(|e| e.to_string())?;
    guard.recent_activities(100).map_err(|e| e.to_string())
}

pub fn run() {
    let app_data = default_app_data_dir();
    let cfg = load_or_create_config(&app_data, default_claude_dir())
        .expect("Failed to load or initialize SessionShare config");
    let engine = SyncEngine::new(app_data, cfg).expect("Failed to initialize SyncEngine");

    let app_state = AppState {
        engine: Arc::new(Mutex::new(engine)),
        watcher: Mutex::new(None),
        suppress_watch: Arc::new(AtomicBool::new(false)),
        is_syncing: Arc::new(AtomicBool::new(false)),
        last_report: Arc::new(Mutex::new(None)),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(app_state)
        .setup(|app| {
            // Start background file watcher if already configured
            let state = app.state::<AppState>();
            restart_watcher(app.handle(), &state);

            // Build Windows system tray menu
            let open_i = MenuItem::with_id(app, "open", "Open Dashboard", true, None::<&str>)?;
            let sync_i = MenuItem::with_id(app, "sync", "Sync Now", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit SessionShare", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_i, &sync_i, &quit_i])?;

            let mut tray_builder = TrayIconBuilder::new()
                .menu(&menu)
                .tooltip("SessionShare — Claude Code Sync")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => {
                        if let Some(win) = app.get_webview_window("main") {
                            let _ = win.show();
                            let _ = win.set_focus();
                        }
                    }
                    "sync" => {
                        let app_handle = app.clone();
                        let state = app.state::<AppState>();
                        let engine = Arc::clone(&state.engine);
                        let suppress = Arc::clone(&state.suppress_watch);
                        let is_syncing = Arc::clone(&state.is_syncing);
                        let last_report = Arc::clone(&state.last_report);
                        thread::spawn(move || {
                            let _ = perform_sync(
                                &app_handle,
                                &engine,
                                &suppress,
                                &is_syncing,
                                &last_report,
                            );
                        });
                    }
                    "quit" => {
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(win) = app.get_webview_window("main") {
                            let _ = win.show();
                            let _ = win.set_focus();
                        }
                    }
                });

            if let Some(icon) = app.default_window_icon() {
                tray_builder = tray_builder.icon(icon.clone());
            }

            let _ = tray_builder.build(app)?;

            // Explicitly show and focus the main window on startup
            // (prevents Windows SW_HIDE inheritance when launched from background shells)
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.unminimize();
                let _ = win.set_focus();
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
            list_projects,
            map_project,
            toggle_project_sync,
            list_sessions,
            run_sync_now,
            get_recent_activities
        ])
        .run(tauri::generate_context!())
        .expect("error while running SessionShare");
}
