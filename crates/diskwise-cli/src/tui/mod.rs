//! `diskwise tui`: browse the disk and act on suggestions in the terminal.

mod app;
mod ui;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use diskwise_core::scan::ScanProgress;
use diskwise_core::{ScanOptions, Snapshot};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use app::{App, Modal};

pub struct Options {
    pub root: PathBuf,
    /// Start from a saved snapshot instead of scanning.
    pub from: Option<PathBuf>,
    pub rules: Option<PathBuf>,
}

pub fn run(opts: Options) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = run_inner(&mut terminal, &opts);
    ratatui::restore();
    result
}

fn run_inner(terminal: &mut DefaultTerminal, opts: &Options) -> Result<()> {
    let mut snap = match &opts.from {
        Some(file) => crate::load_snapshot(file)?,
        None => match scan_with_progress(terminal, &opts.root)? {
            Some(s) => s,
            None => return Ok(()),
        },
    };
    loop {
        let rules = crate::load_rules(snap.platform(), opts.rules.as_deref())?;
        let findings = diskwise_core::analyze(&snap, &rules, crate::now());
        let mut app = App::new(snap, findings);
        if opts.from.is_some() {
            app.status = Some("Showing a saved snapshot. Press r to rescan.".into());
        }

        while !app.quit && !app.rescan_requested {
            terminal.draw(|f| ui::draw(f, &app))?;
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                handle_key(&mut app, key);
            }
        }
        if app.quit {
            return Ok(());
        }
        let root = PathBuf::from(&app.snap.root);
        snap = match scan_with_progress(terminal, &root)? {
            Some(s) => s,
            None => return Ok(()),
        };
    }
}

/// Scan in a background thread while drawing live counts. None if the user quit.
fn scan_with_progress(terminal: &mut DefaultTerminal, root: &Path) -> Result<Option<Snapshot>> {
    let progress = Arc::new(ScanProgress::default());
    let opts = ScanOptions {
        progress: Some(progress.clone()),
        ..Default::default()
    };
    let root_owned = root.to_path_buf();
    let handle = thread::spawn(move || diskwise_core::snapshot(&root_owned, opts, true));
    let started = Instant::now();
    let label = root.display().to_string();
    while !handle.is_finished() {
        terminal.draw(|f| ui::draw_scanning(f, &label, &progress, started.elapsed()))?;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(k) = event::read()?
            && k.kind == KeyEventKind::Press
            && (matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) || is_ctrl_c(&k))
        {
            // The scan thread is abandoned; the process exits right after.
            return Ok(None);
        }
    }
    let snap = handle
        .join()
        .map_err(|_| anyhow::anyhow!("scan thread panicked"))?
        .with_context(|| format!("scanning {}", root.display()))?;
    Ok(Some(snap))
}

fn is_ctrl_c(k: &KeyEvent) -> bool {
    k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL)
}

fn handle_key(app: &mut App, key: KeyEvent) {
    if is_ctrl_c(&key) {
        app.quit = true;
        return;
    }
    if let Some(modal) = app.modal.take() {
        if let Modal::ConfirmTrash { targets, .. } = &modal {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    app.status = Some(app.apply_trash(targets, move_to_trash));
                }
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {
                    app.status = Some("Cancelled.".into())
                }
                _ => app.modal = Some(modal), // keep asking
            }
        }
        // Any key closes help and messages.
        return;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
        KeyCode::PageUp => app.move_selection(-10),
        KeyCode::PageDown => app.move_selection(10),
        KeyCode::Home | KeyCode::Char('g') => app.select_first(),
        KeyCode::End | KeyCode::Char('G') => app.select_last(),
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => app.open(),
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => app.up(),
        KeyCode::Tab | KeyCode::BackTab => app.toggle_tab(),
        KeyCode::Char('s') => app.cycle_sort(),
        KeyCode::Char('d') | KeyCode::Delete => app.request_trash(),
        KeyCode::Char('r') => app.rescan_requested = true,
        KeyCode::Char('?') => app.modal = Some(Modal::Help),
        _ => {}
    }
}

/// Move one path to the system Trash. On macOS this uses NSFileManager rather than
/// asking Finder: no extra permission prompt, and it works over SSH.
fn move_to_trash(path: &Path) -> Result<(), String> {
    #[allow(unused_mut)] // only mutated on macOS
    let mut ctx = trash::TrashContext::default();
    #[cfg(target_os = "macos")]
    {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        ctx.set_delete_method(DeleteMethod::NsFileManager);
    }
    ctx.delete(path).map_err(|e| e.to_string())
}
