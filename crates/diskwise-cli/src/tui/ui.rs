//! Drawing. Everything here reads `App`; nothing changes state.

use std::time::Duration;

use diskwise_core::rules::Safety;
use diskwise_core::scan::ScanProgress;
use diskwise_core::{Finding, NodeKind, human_bytes};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

use super::app::{App, Modal, Tab};

const DETAILS_HEIGHT: u16 = 9;

pub fn safety_style(s: Safety) -> Style {
    match s {
        Safety::Safe => Style::new().fg(Color::Green),
        Safety::Caution => Style::new().fg(Color::Yellow),
        Safety::Review => Style::new().fg(Color::Cyan),
    }
}

pub fn draw(f: &mut Frame, app: &App) {
    let [header, tabs, main, details, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(DETAILS_HEIGHT),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_header(f, app, header);
    draw_tabs(f, app, tabs);
    match app.tab {
        Tab::Browse => {
            draw_browser(f, app, main);
            draw_entry_details(f, app, details);
        }
        Tab::Suggestions => {
            draw_suggestions(f, app, main);
            draw_suggestion_details(f, app, details);
        }
    }
    draw_footer(f, app, footer);
    if let Some(m) = &app.modal {
        draw_modal(f, m);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let disk = app.snap.system.as_ref().and_then(|s| {
        let (cap, free) = match &s.container {
            Some(c) => (c.capacity, c.free),
            None => s.filesystem.map(|u| (u.capacity, u.free))?,
        };
        let used = cap.saturating_sub(free);
        Some(format!(
            "{} free of {} ({:.0}% full)",
            human_bytes(free),
            human_bytes(cap),
            used as f64 / cap.max(1) as f64 * 100.0
        ))
    });
    let [left, right] =
        Layout::horizontal([Constraint::Min(10), Constraint::Length(40)]).areas(area);
    f.render_widget(
        Line::from(vec![
            Span::styled(" diskwise ", Style::new().bold().reversed()),
            Span::raw("  "),
            Span::styled(app.current_path(), Style::new().bold()),
        ]),
        left,
    );
    if let Some(d) = disk {
        f.render_widget(Line::from(d).right_aligned().dim(), right);
    }
}

fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let tab = |label: String, active: bool| {
        if active {
            Span::styled(format!(" {label} "), Style::new().bold().reversed())
        } else {
            Span::raw(format!(" {label} ")).dim()
        }
    };
    let cleanable: u64 = app
        .findings
        .iter()
        .filter(|f| f.reclaimable() && f.safety != Safety::Review)
        .map(|f| f.bytes)
        .sum();
    f.render_widget(
        Line::from(vec![
            tab("Browse".into(), app.tab == Tab::Browse),
            Span::raw(" "),
            tab(
                format!("Suggestions ({})", app.findings.len()),
                app.tab == Tab::Suggestions,
            ),
            Span::raw("   "),
            Span::styled(
                format!(
                    "{} can be cleaned up safely or with a quick step",
                    human_bytes(cleanable)
                ),
                Style::new().fg(Color::Green),
            ),
        ]),
        area,
    );
}

/// A 10-cell bar using eighth blocks for sub-cell precision.
fn bar(fraction: f64, width: usize) -> String {
    const PARTS: [&str; 8] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let eighths = (fraction.clamp(0.0, 1.0) * (width * 8) as f64).round() as usize;
    let mut s = "█".repeat(eighths / 8);
    if !eighths.is_multiple_of(8) {
        s.push_str(PARTS[eighths % 8]);
    }
    format!("{s:<width$}")
}

fn draw_browser(f: &mut Frame, app: &App, area: Rect) {
    let dir = app.current_dir();
    let total = dir.allocated.max(1);
    let rows: Vec<Row> = app
        .entries()
        .iter()
        .map(|e| {
            let frac = e.allocated as f64 / total as f64;
            let name = match e.kind {
                NodeKind::Dir => Span::styled(format!("{}/", e.name), Style::new().bold()),
                NodeKind::File => Span::raw(e.name.clone()),
                NodeKind::Rest => Span::raw(format!("({} smaller items)", e.files + e.dirs)).dim(),
            };
            let tag = app
                .entry_path(e)
                .map(|p| tag_for(app, &p, e.kind))
                .unwrap_or_default();
            Row::new(vec![
                Cell::from(Line::from(human_bytes(e.allocated)).right_aligned()),
                Cell::from(bar(frac, 10)).fg(Color::Blue),
                Cell::from(Line::from(format!("{:.1}%", frac * 100.0)).right_aligned()).dim(),
                Cell::from(name),
                Cell::from(tag),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Fill(3),
            Constraint::Fill(2),
        ],
    )
    .block(Block::bordered().title(format!(
        " {} · {} files · sorted by {} ",
        human_bytes(dir.allocated),
        dir.files,
        app.sort.label()
    )))
    .row_highlight_style(Style::new().reversed())
    .column_spacing(1);
    let mut state = TableState::default().with_selected(Some(app.selected));
    f.render_stateful_widget(table, area, &mut state);
}

/// Right-hand tag for a browser row: the suggestion covering it, or how much is
/// cleanable inside a folder.
fn tag_for(app: &App, path: &str, kind: NodeKind) -> Line<'static> {
    if let Some(c) = app.coverage(path).filter(|c| c.direct) {
        return Line::from(vec![
            Span::styled("● ", safety_style(c.finding.safety)),
            Span::styled(c.finding.title.clone(), safety_style(c.finding.safety)),
        ]);
    }
    let inside = app.cleanable_below(path);
    if kind == NodeKind::Dir && inside > 0 {
        return Line::from(format!("{} cleanable inside", human_bytes(inside)))
            .fg(Color::Green)
            .dim();
    }
    Line::default()
}

fn age(newest_mtime: i64, now: i64) -> String {
    if newest_mtime <= 0 {
        return "unknown".into();
    }
    let days = (now - newest_mtime).max(0) / 86_400;
    match days {
        0 => "today".into(),
        1 => "yesterday".into(),
        2..=60 => format!("{days} days ago"),
        61..=730 => format!("{} months ago", days / 30),
        _ => format!("{} years ago", days / 365),
    }
}

fn finding_lines(f: &Finding) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled(format!("{} ", f.safety), safety_style(f.safety).bold()),
            Span::styled(f.title.clone(), Style::new().bold()),
            Span::raw(format!("   [{} · {}]", f.category, f.action)).dim(),
        ]),
        Line::from(f.why.clone()),
        Line::from(format!("→ {}", f.how)),
    ];
    if let Some(p) = &f.prevent {
        lines.push(Line::from(format!("Prevent: {p}")).dim());
    }
    lines
}

fn draw_entry_details(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered().title(" Details ");
    let Some(e) = app.selected_node() else {
        f.render_widget(Paragraph::new("Empty folder.").block(block), area);
        return;
    };
    let now = app.snap.scanned_at;
    let mut lines = Vec::new();
    match app.entry_path(e) {
        Some(path) => {
            lines.push(Line::from(path.clone()).bold());
            let mut facts = format!("{} on disk", human_bytes(e.allocated));
            if e.kind == NodeKind::Dir {
                facts.push_str(&format!(" · {} files · {} folders", e.files, e.dirs));
            }
            facts.push_str(&format!(" · last changed {}", age(e.newest_mtime, now)));
            if e.incomplete {
                facts.push_str(" · partly unreadable");
            }
            lines.push(Line::from(facts).dim());
            match app.coverage(&path) {
                Some(c) if c.direct => lines.extend(finding_lines(c.finding)),
                Some(c) => lines.push(Line::from(vec![
                    Span::raw("Inside "),
                    Span::styled(c.finding.title.clone(), safety_style(c.finding.safety)),
                    Span::raw(format!(" ({}): {}", c.finding.safety, c.finding.how)),
                ])),
                None => lines.push(Line::from("No suggestion covers this item.").dim()),
            }
        }
        None => lines.push(Line::from(format!(
            "{} files and {} folders, each smaller than {}.",
            e.files,
            e.dirs,
            human_bytes(app.snap.min_node_size)
        ))),
    }
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(block),
        area,
    );
}

fn tilde(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            let rest = &path[home.len()..];
            if rest.is_empty() || rest.starts_with('/') {
                format!("~{rest}")
            } else {
                path.into()
            }
        }
        _ => path.into(),
    }
}

fn draw_suggestions(f: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .findings
        .iter()
        .map(|fd| {
            let size = if fd.size_known {
                human_bytes(fd.bytes)
            } else {
                "size ?".into()
            };
            let location = match fd.items.as_slice() {
                [] => String::new(),
                [one] => tilde(&one.path),
                [first, rest @ ..] => format!("{} (+{} more)", tilde(&first.path), rest.len()),
            };
            Row::new(vec![
                Cell::from(Line::from(size).right_aligned()),
                Cell::from(fd.safety.to_string()).style(safety_style(fd.safety)),
                Cell::from(fd.title.clone()),
                Cell::from(fd.action.to_string()).dim(),
                Cell::from(location).dim(),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Length(7),
            Constraint::Fill(3),
            Constraint::Length(20),
            Constraint::Fill(4),
        ],
    )
    .block(Block::bordered().title(" Suggestions · safest first "))
    .row_highlight_style(Style::new().reversed())
    .column_spacing(1);
    let mut state = TableState::default().with_selected(Some(app.suggestion));
    f.render_stateful_widget(table, area, &mut state);
}

fn draw_suggestion_details(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered().title(" Details ");
    let Some(fd) = app.selected_finding() else {
        f.render_widget(Paragraph::new("No suggestions.").block(block), area);
        return;
    };
    let mut lines = finding_lines(fd);
    for item in fd.items.iter().take(3) {
        let from = item
            .source
            .as_ref()
            .map(|h| format!("  (from {h})"))
            .unwrap_or_default();
        lines.push(
            Line::from(format!(
                "  {:>9}  {}{from}",
                human_bytes(item.bytes),
                tilde(&item.path)
            ))
            .dim(),
        );
    }
    if fd.items.len() > 3 {
        lines.push(Line::from(format!("  … and {} more", fd.items.len() - 3)).dim());
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let line = match &app.status {
        Some(s) => Line::from(s.clone()).fg(Color::Yellow),
        None => {
            let keys = match app.tab {
                Tab::Browse => {
                    "↑↓ move  ⏎/→ open  ← up  tab suggestions  d trash  s sort  r rescan  ? help  q quit"
                }
                Tab::Suggestions => {
                    "↑↓ move  ⏎ show in browser  tab browse  d trash all  r rescan  ? help  q quit"
                }
            };
            Line::from(keys).dim()
        }
    };
    f.render_widget(line, area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [h] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [v] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(h);
    v
}

fn draw_modal(f: &mut Frame, modal: &Modal) {
    let (title, text, border) = match modal {
        Modal::Help => (
            " Keys ".to_string(),
            Text::from(vec![
                Line::from("↑ ↓  j k        move            PgUp PgDn   move faster"),
                Line::from("⏎ → l           open folder / show suggestion in browser"),
                Line::from("← h Backspace   up one folder"),
                Line::from("Tab             switch Browse / Suggestions"),
                Line::from("d Delete        move to Trash (asks first, never deletes outright)"),
                Line::from("s               sort: size → name → oldest first"),
                Line::from("r               rescan"),
                Line::from("q Esc           quit"),
                Line::from(""),
                Line::from(vec![
                    Span::styled("● safe", safety_style(Safety::Safe)),
                    Span::raw(" nothing is lost   "),
                    Span::styled("● caution", safety_style(Safety::Caution)),
                    Span::raw(" safe after a quick step   "),
                    Span::styled("● review", safety_style(Safety::Review)),
                    Span::raw(" your call"),
                ]),
            ]),
            Color::Reset,
        ),
        Modal::Message { title, body } => (
            format!(" {title} "),
            Text::from(
                body.lines()
                    .map(|l| Line::from(l.to_string()))
                    .collect::<Vec<_>>(),
            ),
            Color::Cyan,
        ),
        Modal::ConfirmTrash { targets, warning } => {
            let total: u64 = targets.iter().map(|t| t.bytes).sum();
            let mut lines = Vec::new();
            for t in targets.iter().take(8) {
                lines.push(Line::from(format!(
                    "{:>9}  {}",
                    human_bytes(t.bytes),
                    t.path
                )));
            }
            if targets.len() > 8 {
                lines
                    .push(Line::from(format!("           … and {} more", targets.len() - 8)).dim());
            }
            lines.push(Line::from(""));
            lines.push(Line::from(format!("{} in total.", human_bytes(total))).bold());
            if let Some(w) = warning {
                lines.push(Line::from(""));
                lines.push(Line::from(w.clone()).fg(Color::Yellow));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(" y ", Style::new().reversed()),
                Span::raw(" move to Trash    "),
                Span::styled(" n ", Style::new().reversed()),
                Span::raw(" cancel"),
            ]));
            (
                " Move to Trash? ".to_string(),
                Text::from(lines),
                Color::Yellow,
            )
        }
    };
    // Border plus a little room for lines that wrap at this width.
    let area = centered(f.area(), 90, text.height() as u16 + 3);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(
                Block::bordered()
                    .title(title)
                    .border_style(Style::new().fg(border)),
            )
            .style(Style::new().add_modifier(Modifier::empty())),
        area,
    );
}

pub fn draw_scanning(f: &mut Frame, root: &str, p: &ScanProgress, elapsed: Duration) {
    use std::sync::atomic::Ordering::Relaxed;
    let text = Text::from(vec![
        Line::from(format!("Scanning {root} …")).bold(),
        Line::from(""),
        Line::from(format!(
            "{} files · {} folders · {}",
            p.files.load(Relaxed),
            p.dirs.load(Relaxed),
            human_bytes(p.bytes.load(Relaxed))
        )),
        Line::from(format!("{:.0}s", elapsed.as_secs_f64())).dim(),
        Line::from(""),
        Line::from("q to quit").dim(),
    ]);
    let area = centered(f.area(), 60, 8);
    f.render_widget(
        Paragraph::new(text)
            .centered()
            .block(Block::bordered().title(" diskwise ")),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        let buf = t.backend().buffer();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Visual check against a real snapshot:
    /// DISKWISE_SNAPSHOT=snap.json cargo test -p diskwise-cli render_real -- --ignored --nocapture
    #[test]
    #[ignore]
    fn render_real_snapshot() {
        let path = std::env::var("DISKWISE_SNAPSHOT").expect("set DISKWISE_SNAPSHOT");
        let snap = crate::load_snapshot(std::path::Path::new(&path)).unwrap();
        let rules = crate::load_rules(snap.platform(), None).unwrap();
        let findings = diskwise_core::analyze(&snap, &rules, crate::now());
        let mut app = App::new(snap, findings);
        let home = std::env::var("HOME").unwrap();
        app.reveal(&format!("{home}/Downloads"));
        app.open();
        println!("{}\n", render(&app, 120, 32));
        app.toggle_tab();
        println!("{}\n", render(&app, 120, 32));
        app.reveal(&format!("{home}/Library/Caches"));
        app.open();
        app.request_trash();
        println!("{}", render(&app, 120, 32));
    }

    #[test]
    fn renders_browser_and_suggestions() {
        let mut app = super::super::app::sample_app("/r");
        app.reveal("/r/home/v/cache");
        let screen = render(&app, 100, 24);
        assert!(screen.contains("/r/home/v"), "{screen}");
        assert!(
            screen.contains("cache/") && screen.contains("● cache"),
            "{screen}"
        );
        app.toggle_tab();
        let screen = render(&app, 100, 24);
        assert!(
            screen.contains("Suggestions (3)") && screen.contains("safe"),
            "{screen}"
        );
        app.request_trash();
        let screen = render(&app, 100, 24);
        assert!(
            screen.contains("Move to Trash?") && screen.contains("/r/home/v/cache"),
            "{screen}"
        );
    }
}
