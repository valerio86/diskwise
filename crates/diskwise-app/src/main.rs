//! diskwise desktop app: a Tauri shell around `diskwise-core`.
//!
//! Usage: `diskwise-app [PATH]` scans PATH (default: the whole disk);
//! `diskwise-app --from snapshot.json` opens a saved scan instead.

// No console window on Windows release builds (harmless elsewhere).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod session;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use diskwise_core::scan::ScanProgress;
use diskwise_core::{DEFAULT_ROOT, RuleSet, ScanOptions, Snapshot};
use serde::Serialize;
use session::{Plan, Session, Summary, TrashResult, View};
use tauri::{AppHandle, Emitter, State};

/// Where to get data from, decided by the command line.
enum Source {
    Scan(PathBuf),
    Saved(PathBuf),
}

struct AppState {
    source: Source,
    session: Mutex<Option<Session>>,
    scanning: AtomicBool,
}

#[derive(Clone, Serialize)]
struct Progress {
    root: String,
    files: u64,
    dirs: u64,
    bytes: u64,
    seconds: f64,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ScanDone {
    Ready,
    Failed { message: String },
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn open_session(snap: Snapshot) -> Result<Session, String> {
    let rules = RuleSet::builtin_for(snap.platform()).map_err(|e| format!("{e:#}"))?;
    Ok(Session::new(snap, rules, now()))
}

/// Start a scan (or load the saved snapshot) in the background. Progress arrives
/// as `scan-progress` events and the end as one `scan-done` event.
#[tauri::command]
fn start_scan(app: AppHandle, state: State<'_, Arc<AppState>>, rescan: bool) -> Result<(), String> {
    if state.scanning.swap(true, Ordering::SeqCst) {
        return Err("A scan is already running.".into());
    }
    let state = Arc::clone(&state);
    std::thread::spawn(move || {
        let result = match (&state.source, rescan) {
            (Source::Saved(file), false) => std::fs::read(file)
                .map_err(|e| format!("Couldn't read {}: {e}", file.display()))
                .and_then(|b| {
                    serde_json::from_slice::<Snapshot>(&b)
                        .map_err(|e| format!("{} isn't a diskwise snapshot: {e}", file.display()))
                }),
            (source, _) => {
                let root = match source {
                    Source::Scan(p) => p.clone(),
                    // Rescanning a saved snapshot scans the folder it came from.
                    Source::Saved(_) => state
                        .session
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|s| PathBuf::from(&s.snap.root))
                        .unwrap_or_else(|| PathBuf::from(DEFAULT_ROOT)),
                };
                scan_with_progress(&app, &root)
            }
        };
        let done = match result.and_then(open_session) {
            Ok(session) => {
                *state.session.lock().unwrap() = Some(session);
                ScanDone::Ready
            }
            Err(message) => ScanDone::Failed { message },
        };
        state.scanning.store(false, Ordering::SeqCst);
        let _ = app.emit("scan-done", done);
    });
    Ok(())
}

fn scan_with_progress(app: &AppHandle, root: &std::path::Path) -> Result<Snapshot, String> {
    let progress = Arc::new(ScanProgress::default());
    let opts = ScanOptions {
        progress: Some(progress.clone()),
        ..Default::default()
    };
    let root_owned = root.to_path_buf();
    let worker = std::thread::spawn(move || diskwise_core::snapshot(&root_owned, opts, true));
    let started = Instant::now();
    let label = root.display().to_string();
    while !worker.is_finished() {
        let _ = app.emit(
            "scan-progress",
            Progress {
                root: label.clone(),
                files: progress.files.load(Ordering::Relaxed),
                dirs: progress.dirs.load(Ordering::Relaxed),
                bytes: progress.bytes.load(Ordering::Relaxed),
                seconds: started.elapsed().as_secs_f64(),
            },
        );
        std::thread::sleep(Duration::from_millis(120));
    }
    worker
        .join()
        .map_err(|_| "The scan stopped unexpectedly.".to_string())?
        .map_err(|e| format!("Couldn't scan {}: {e}", root.display()))
}

fn with_session<T>(state: &AppState, f: impl FnOnce(&mut Session) -> T) -> Result<T, String> {
    let mut guard = state.session.lock().unwrap();
    let session = guard.as_mut().ok_or("No scan yet.")?;
    Ok(f(session))
}

#[tauri::command]
fn summary(state: State<'_, Arc<AppState>>) -> Result<Summary, String> {
    with_session(&state, |s| s.summary())
}

#[tauri::command]
fn view(state: State<'_, Arc<AppState>>, path: Option<String>) -> Result<View, String> {
    with_session(&state, |s| s.view(path.as_deref()))?
        .ok_or_else(|| "That folder isn't in the scan.".into())
}

#[tauri::command]
fn plan_path(state: State<'_, Arc<AppState>>, path: String) -> Result<Plan, String> {
    with_session(&state, |s| s.plan_path(&path))
}

#[tauri::command]
fn plan_finding(state: State<'_, Arc<AppState>>, index: usize) -> Result<Plan, String> {
    with_session(&state, |s| s.plan_finding(index))
}

#[tauri::command]
fn trash(state: State<'_, Arc<AppState>>, paths: Vec<String>) -> Result<TrashResult, String> {
    with_session(&state, |s| s.trash_for_real(&paths))
}

/// Show an item in Finder (macOS) or its folder in the file manager (Linux).
#[tauri::command]
fn reveal(state: State<'_, Arc<AppState>>, path: String) -> Result<(), String> {
    let real = with_session(&state, |s| s.snap.real_path(&path))?;
    #[cfg(target_os = "macos")]
    let status = std::process::Command::new("open")
        .args(["-R", &real])
        .status();
    #[cfg(not(target_os = "macos"))]
    let status = std::process::Command::new("xdg-open")
        .arg(
            std::path::Path::new(&real)
                .parent()
                .unwrap_or(std::path::Path::new("/")),
        )
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(_) | Err(_) => Err(format!("Couldn't show {path}.")),
    }
}

fn parse_args() -> Source {
    let mut args = std::env::args().skip(1);
    match (args.next(), args.next()) {
        (Some(flag), Some(file)) if flag == "--from" => Source::Saved(PathBuf::from(file)),
        (Some(path), None) if !path.starts_with('-') => Source::Scan(PathBuf::from(path)),
        _ => Source::Scan(PathBuf::from(DEFAULT_ROOT)),
    }
}

fn main() {
    let state = Arc::new(AppState {
        source: parse_args(),
        session: Mutex::new(None),
        scanning: AtomicBool::new(false),
    });
    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            start_scan,
            summary,
            view,
            plan_path,
            plan_finding,
            trash,
            reveal
        ])
        .run(tauri::generate_context!())
        .expect("error while running diskwise");
}
