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

/// `live`: scanned in this run (so the disk matches it and trashing is allowed),
/// as opposed to loaded from a saved file.
fn open_session(snap: Snapshot, live: bool) -> Result<Session, String> {
    let rules = RuleSet::builtin_for(snap.platform()).map_err(|e| format!("{e:#}"))?;
    Ok(Session::new(snap, rules, now(), live))
}

fn load_saved(file: &std::path::Path) -> Result<Snapshot, String> {
    std::fs::read(file)
        .map_err(|e| format!("Couldn't read {}: {e}", file.display()))
        .and_then(|b| {
            serde_json::from_slice::<Snapshot>(&b)
                .map_err(|e| format!("{} isn't a diskwise snapshot: {e}", file.display()))
        })
}

/// Start a scan (or load the saved snapshot) in the background. Progress arrives
/// as `scan-progress` events and the end as one `scan-done` event.
#[tauri::command]
async fn start_scan(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    rescan: bool,
) -> Result<(), String> {
    if state.scanning.swap(true, Ordering::SeqCst) {
        return Err("A scan is already running.".into());
    }
    let state = Arc::clone(&state);
    std::thread::spawn(move || {
        // The folder an earlier scan or snapshot covered, if any.
        let scanned_root = lock(&state.session)
            .as_ref()
            .map(|s| PathBuf::from(&s.snap.root));
        let result = match (&state.source, rescan, scanned_root) {
            // Rescanning a saved snapshot scans the folder it came from…
            (Source::Saved(_), true, Some(root)) => {
                scan_with_progress(&app, &root).and_then(|s| open_session(s, true))
            }
            // …but if the file never loaded, there's no folder: retry the file.
            (Source::Saved(file), _, _) => load_saved(file).and_then(|s| open_session(s, false)),
            (Source::Scan(root), _, _) => {
                scan_with_progress(&app, root).and_then(|s| open_session(s, true))
            }
        };
        let done = match result {
            Ok(session) => {
                *lock(&state.session) = Some(session);
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

/// Lock, recovering from a panic in an earlier command instead of failing every
/// command after it (the session data is still consistent: each command either
/// finishes its update or doesn't start it).
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn with_session<T>(state: &AppState, f: impl FnOnce(&mut Session) -> T) -> Result<T, String> {
    let mut guard = lock(&state.session);
    let session = guard.as_mut().ok_or("No scan yet.")?;
    Ok(f(session))
}

// Commands are async so Tauri runs them off the main thread: the window keeps
// responding while they work.

#[tauri::command]
async fn summary(state: State<'_, Arc<AppState>>) -> Result<Summary, String> {
    with_session(&state, |s| s.summary())
}

#[tauri::command]
async fn view(state: State<'_, Arc<AppState>>, path: Option<String>) -> Result<View, String> {
    with_session(&state, |s| s.view(path.as_deref()))?
        .ok_or_else(|| "That folder isn't in the scan.".into())
}

#[tauri::command]
async fn plan_path(state: State<'_, Arc<AppState>>, path: String) -> Result<Plan, String> {
    with_session(&state, |s| s.plan_path(&path))
}

/// Suggestions are named by id: positions change when they re-sort.
#[tauri::command]
async fn plan_finding(state: State<'_, Arc<AppState>>, id: String) -> Result<Plan, String> {
    with_session(&state, |s| s.plan_finding(&id))
}

/// Moving to the Trash can take a while (copies across filesystems on Linux), so
/// it runs on a blocking-friendly worker thread.
#[tauri::command]
async fn trash(state: State<'_, Arc<AppState>>, paths: Vec<String>) -> Result<TrashResult, String> {
    let state = Arc::clone(&state);
    tauri::async_runtime::spawn_blocking(move || with_session(&state, |s| s.trash_for_real(&paths)))
        .await
        .map_err(|e| format!("Moving to the Trash stopped unexpectedly: {e}"))?
}

/// Show an item in Finder (macOS) or its folder in the file manager (Linux).
#[tauri::command]
async fn reveal(state: State<'_, Arc<AppState>>, path: String) -> Result<(), String> {
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
