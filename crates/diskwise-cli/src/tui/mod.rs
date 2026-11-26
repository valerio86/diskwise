//! `diskwise tui`: browse the disk and act on suggestions in the terminal.

mod app;
pub mod theme;
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
    pub color: theme::ColorMode,
    /// Also descend into other mounted filesystems.
    pub cross_mounts: bool,
}

pub fn run(opts: Options) -> Result<()> {
    theme::init(opts.color);
    let mut terminal = ratatui::init();
    let result = run_inner(&mut terminal, &opts);
    ratatui::restore();
    result
}

fn run_inner(terminal: &mut DefaultTerminal, opts: &Options) -> Result<()> {
    let mut snap = match &opts.from {
        Some(file) => crate::load_snapshot(file)?,
        None => match scan_with_progress(terminal, &opts.root, opts.cross_mounts)? {
            Some(s) => s,
            None => return Ok(()),
        },
    };
    let mut from_saved = opts.from.is_some();
    loop {
        let rules = crate::load_rules(snap.platform(), opts.rules.as_deref())?;
        let findings = diskwise_core::analyze(&snap, &rules, crate::now());
        let mut app = App::new(snap, findings);
        if from_saved {
            app.status = Some(
                "Showing a saved scan: press r to scan again before moving anything to the Trash."
                    .into(),
            );
            app.live = false;
            from_saved = false;
        }
        app.start_dupes();

        while !app.quit && !app.rescan_requested {
            terminal.draw(|f| ui::draw(f, &app))?;
            // While duplicates are being checked, wake up to redraw progress.
            let ready = !app.dupes_checking() || event::poll(Duration::from_millis(150))?;
            if ready
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                handle_key(&mut app, key);
            }
            app.poll_dupes();
        }
        if app.quit {
            return Ok(());
        }
        let root = PathBuf::from(&app.snap.root);
        snap = match scan_with_progress(terminal, &root, opts.cross_mounts)? {
            Some(s) => s,
            None => return Ok(()),
        };
    }
}

/// Scan in a background thread while drawing live counts. None if the user quit.
fn scan_with_progress(
    terminal: &mut DefaultTerminal,
    root: &Path,
    cross_mounts: bool,
) -> Result<Option<Snapshot>> {
    let progress = Arc::new(ScanProgress::default());
    let opts = ScanOptions {
        progress: Some(progress.clone()),
        one_filesystem: !cross_mounts,
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
        let yes = matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'));
        let no = matches!(
            key.code,
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q')
        );
        match modal {
            Modal::ConfirmTrash { ref targets, .. } => {
                if yes {
                    app.status =
                        Some(app.apply_trash(targets, diskwise_core::actions::move_to_trash));
                    // What's left may have changed: check duplicates again.
                    app.start_dupes();
                } else if no {
                    app.status = Some("Cancelled.".into())
                } else {
                    app.modal = Some(modal); // keep asking
                }
            }
            Modal::ConfirmDupes { ref plan, .. } => {
                if yes {
                    app.status = Some(app.apply_dupes(plan, diskwise_core::actions::move_to_trash));
                } else if no {
                    app.status = Some("Cancelled.".into())
                } else {
                    app.modal = Some(modal);
                }
            }
            Modal::PreferFolder {
                ref prefer,
                ref over,
            } => {
                if yes {
                    app.remember_folder_order(prefer, over);
                } else if no {
                    app.status = Some("Keeping that copy this time only.".into())
                } else {
                    app.modal = Some(modal);
                }
            }
            Modal::ChooseFolder { options, selected } => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    app.modal = Some(Modal::ChooseFolder {
                        selected: selected.saturating_sub(1),
                        options,
                    })
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.modal = Some(Modal::ChooseFolder {
                        selected: (selected + 1).min(options.len() - 1),
                        options,
                    })
                }
                KeyCode::Enter => app.mark_intentional(&options[selected]),
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n') => {
                    app.status = Some("Cancelled.".into())
                }
                _ => app.modal = Some(Modal::ChooseFolder { options, selected }),
            },
            // Any key closes help and messages.
            Modal::Help | Modal::Message { .. } => {}
        }
        return;
    }
    if app.tab == app::Tab::Duplicates {
        match key.code {
            KeyCode::Left | KeyCode::Char('h') => return app.move_copy(-1),
            KeyCode::Right | KeyCode::Char('l') => return app.move_copy(1),
            KeyCode::Char(' ') => return app.cycle_action(),
            KeyCode::Char('p') => return app.make_keeper(),
            KeyCode::Char('i') => return app.request_intentional(),
            _ => {}
        }
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
        KeyCode::Tab => app.toggle_tab(),
        KeyCode::BackTab => app.toggle_tab_back(),
        KeyCode::Char('1') => app.tab = app::Tab::Suggestions,
        KeyCode::Char('2') => app.tab = app::Tab::Browse,
        KeyCode::Char('3') => app.tab = app::Tab::Duplicates,
        KeyCode::Char('s') => app.cycle_sort(),
        KeyCode::Char('d') | KeyCode::Delete => app.request_trash(),
        KeyCode::Char('r') => app.rescan_requested = true,
        KeyCode::Char('?') => app.modal = Some(Modal::Help),
        _ => {}
    }
}
