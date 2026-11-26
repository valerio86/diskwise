//! Drawing. Everything here reads `App`; nothing changes state.
//!
//! The look follows Claude Code's own dialogs: brand and tabs on one line with
//! the active tab filled in the accent colour, bold headings with muted detail
//! lines instead of boxes, and bars drawn as an accent fill on a slate track.

use std::time::Duration;

use diskwise_core::rules::Safety;
use diskwise_core::scan::ScanProgress;
use diskwise_core::{Finding, NodeKind, human_bytes};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, HighlightSpacing, Paragraph, Row, Table, TableState,
    Wrap,
};

use super::app::{App, Modal, Tab};
use super::theme::th;

const DETAILS_HEIGHT: u16 = 8;
const BAR_WIDTH: usize = 12;

pub fn safety_style(s: Safety) -> Style {
    Style::new().fg(match s {
        Safety::Safe => th().safe,
        Safety::Caution => th().caution,
        Safety::Review => th().review,
    })
}

fn muted() -> Style {
    Style::new().fg(th().muted)
}

fn accent() -> Style {
    Style::new().fg(th().accent)
}

/// A section separator with a bold heading: `── Details ─────`.
fn section(title: &str) -> Block<'static> {
    Block::new()
        .borders(Borders::TOP)
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(th().rule))
        .title(Span::styled(format!(" {title} "), Style::new().bold()))
        .title_style(Style::new().fg(ratatui::style::Color::Reset))
        .padding(ratatui::widgets::Padding::horizontal(1))
}

/// Key hints: the key in the accent colour, its label muted.
fn key_hints(pairs: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw(" ")];
    for (i, (key, label)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(key.to_string(), accent().bold()));
        spans.push(Span::styled(format!(" {label}"), muted()));
    }
    spans
}

pub fn draw(f: &mut Frame, app: &App) {
    let [tabs, _, location, main, details, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(DETAILS_HEIGHT),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_tabs(f, app, tabs);
    draw_location(f, app, location);
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

/// `diskwise   Browse   Suggestions · 24          20.5 GB cleanable now`
fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let tab = |label: String, active: bool| {
        if active {
            Span::styled(format!(" {label} "), th().filled(th().accent))
        } else {
            Span::styled(format!(" {label} "), Style::new().bold())
        }
    };
    let cleanable: u64 = app
        .findings
        .iter()
        .filter(|f| f.reclaimable() && f.safety != Safety::Review)
        .map(|f| f.bytes)
        .sum();
    let [left, right] =
        Layout::horizontal([Constraint::Min(30), Constraint::Length(30)]).areas(area);
    f.render_widget(
        Line::from(vec![
            Span::styled(" diskwise ", accent().bold()),
            Span::raw("  "),
            tab(
                format!("Suggestions · {}", app.findings.len()),
                app.tab == Tab::Suggestions,
            ),
            Span::raw("  "),
            tab("Browse".into(), app.tab == Tab::Browse),
            Span::styled("   tab to switch", muted()),
        ]),
        left,
    );
    f.render_widget(
        Line::from(vec![
            Span::styled(human_bytes(cleanable), Style::new().fg(th().safe).bold()),
            Span::styled(" cleanable now ", muted()),
        ])
        .right_aligned(),
        right,
    );
}

/// Bold heading (where we are) with a muted line under it (what's here), plus the
/// disk's fill level on the right as a bar, like Claude Code's usage meters.
fn draw_location(f: &mut Frame, app: &App, area: Rect) {
    let [left, right] = Layout::horizontal([
        Constraint::Min(20),
        Constraint::Length(BAR_WIDTH as u16 + 28),
    ])
    .areas(area);
    let (title, subtitle) = match app.tab {
        Tab::Browse => {
            let dir = app.current_dir();
            (
                app.current_path(),
                format!(
                    "{} · {} files · sorted by {}",
                    human_bytes(dir.allocated),
                    dir.files,
                    app.sort.label()
                ),
            )
        }
        Tab::Suggestions => (
            "What you could clean up".to_string(),
            "Safest first · ⏎ shows an item in the browser".to_string(),
        ),
    };
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(format!(" {title}"), Style::new().bold())),
            Line::from(Span::styled(format!(" {subtitle}"), muted())),
        ]),
        left,
    );

    let disk = app.snap.system.as_ref().and_then(|s| match &s.container {
        Some(c) => Some((c.capacity, c.free)),
        None => s.filesystem.map(|u| (u.capacity, u.free)),
    });
    if let Some((cap, free)) = disk {
        let used = cap.saturating_sub(free);
        let frac = used as f64 / cap.max(1) as f64;
        let mut bar_line = bar(frac, BAR_WIDTH);
        bar_line.push(Span::styled(
            format!(" {:.0}% full", frac * 100.0),
            Style::new().bold(),
        ));
        f.render_widget(
            Paragraph::new(vec![
                Line::from(bar_line).right_aligned(),
                Line::from(Span::styled(
                    format!("{} free of {} ", human_bytes(free), human_bytes(cap)),
                    muted(),
                ))
                .right_aligned(),
            ]),
            right,
        );
    }
}

/// A bar as an accent fill on a slate track, with eighth-block precision at the
/// edge. Colours are set per span (including background), so the bar looks the
/// same on the selected row.
fn bar(fraction: f64, width: usize) -> Vec<Span<'static>> {
    const PARTS: [&str; 8] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    if !th().has_color() {
        let full = (fraction.clamp(0.0, 1.0) * width as f64).round() as usize;
        return vec![Span::raw(format!(
            "{}{}",
            "█".repeat(full),
            "░".repeat(width - full)
        ))];
    }
    let eighths = (fraction.clamp(0.0, 1.0) * (width * 8) as f64).round() as usize;
    let full = eighths / 8;
    let mut spans = vec![Span::styled(" ".repeat(full), Style::new().bg(th().accent))];
    let mut used = full;
    if !eighths.is_multiple_of(8) {
        spans.push(Span::styled(
            PARTS[eighths % 8],
            Style::new().fg(th().accent).bg(th().track),
        ));
        used += 1;
    }
    spans.push(Span::styled(
        " ".repeat(width.saturating_sub(used)),
        Style::new().bg(th().track),
    ));
    spans
}

/// Selection is drawn as the selected row's base style (see `Theme::selected`),
/// so the highlight itself only adds the pointer.
fn selection_table<'a>(table: Table<'a>) -> Table<'a> {
    table
        .row_highlight_style(Style::new())
        .highlight_symbol(Span::styled("❯ ", accent().bold()))
        .highlight_spacing(HighlightSpacing::Always)
        .column_spacing(2)
}

fn draw_browser(f: &mut Frame, app: &App, area: Rect) {
    let total = app.current_dir().allocated.max(1);
    let rows: Vec<Row> = app
        .entries()
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let frac = e.allocated as f64 / total as f64;
            let name = match e.kind {
                NodeKind::Dir => {
                    Line::from(Span::styled(format!("{}/", e.name), Style::new().bold()))
                }
                NodeKind::File => Line::from(e.name.clone()),
                NodeKind::Rest => Line::from(Span::styled(
                    format!("({} smaller items)", e.files + e.dirs),
                    muted(),
                )),
                // Links: italic and muted, with where they point.
                NodeKind::Link => Line::from(vec![
                    Span::styled(e.name.clone(), muted().italic()),
                    Span::styled(
                        format!(" → {}", e.target.as_deref().unwrap_or("?")),
                        muted().italic(),
                    ),
                ]),
            };
            let tag = app
                .entry_path(e)
                .map(|p| tag_for(app, &p, e.kind))
                .unwrap_or_default();
            Row::new(vec![
                Cell::from(Line::from(human_bytes(e.allocated)).right_aligned()),
                Cell::from(Line::from(bar(frac, BAR_WIDTH))),
                Cell::from(Line::from(format!("{:.1}%", frac * 100.0)).right_aligned())
                    .style(muted()),
                Cell::from(name),
                Cell::from(tag),
            ])
            .style(if i == app.selected {
                th().selected()
            } else {
                Style::new()
            })
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Length(BAR_WIDTH as u16),
            Constraint::Length(6),
            Constraint::Fill(3),
            Constraint::Fill(2),
        ],
    );
    let mut state = TableState::default().with_selected(Some(app.selected));
    f.render_stateful_widget(selection_table(table), area, &mut state);
}

/// Right-hand tag for a browser row: the suggestion covering it, or how much is
/// cleanable inside a folder.
fn tag_for(app: &App, path: &str, kind: NodeKind) -> Line<'static> {
    if let Some(c) = app.coverage(path).filter(|c| c.direct) {
        let style = safety_style(c.finding.safety);
        return Line::from(vec![
            Span::styled("● ", style),
            Span::styled(c.finding.title.clone(), style),
        ]);
    }
    let inside = app.cleanable_below(path);
    if kind == NodeKind::Dir && inside > 0 {
        return Line::from(vec![
            Span::styled(human_bytes(inside), Style::new().fg(th().safe)),
            Span::styled(" cleanable inside", muted()),
        ]);
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

/// Text with `commands` in backticks: commands drawn in the accent colour (bold,
/// which also shows without colour), backticks dropped. A terminal can't offer a
/// Copy button, but the command stands out and can be selected.
fn rich(text: &str, base: Style) -> Vec<Span<'static>> {
    text.split('`')
        .enumerate()
        .filter(|(_, part)| !part.is_empty())
        .map(|(i, part)| {
            if i % 2 == 1 {
                Span::styled(part.to_string(), accent().bold())
            } else {
                Span::styled(part.to_string(), base)
            }
        })
        .collect()
}

fn finding_lines(f: &Finding) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled(format!("● {} ", f.safety), safety_style(f.safety).bold()),
            Span::styled(f.title.clone(), Style::new().bold()),
            Span::styled(format!("   {} · {}", f.category, f.action), muted()),
        ]),
        Line::from(f.why.clone()),
        Line::from(
            [
                vec![Span::styled("→ ", accent().bold())],
                rich(&f.how, Style::new()),
            ]
            .concat(),
        ),
    ];
    if let Some(p) = &f.prevent {
        lines.push(Line::from(
            [vec![Span::styled("Prevent: ", muted())], rich(p, muted())].concat(),
        ));
    }
    lines
}

fn draw_entry_details(f: &mut Frame, app: &App, area: Rect) {
    let block = section("Details");
    let Some(e) = app.selected_node() else {
        f.render_widget(Paragraph::new(" Empty folder.").block(block), area);
        return;
    };
    let now = app.snap.scanned_at;
    let mut lines = Vec::new();
    match app.entry_path(e) {
        Some(path) => {
            let mut facts = format!("{} on disk", human_bytes(e.allocated));
            if e.kind == NodeKind::Dir {
                facts.push_str(&format!(" · {} files · {} folders", e.files, e.dirs));
            }
            facts.push_str(&format!(" · last changed {}", age(e.newest_mtime, now)));
            if e.incomplete {
                facts.push_str(" · partly unreadable");
            }
            lines.push(Line::from(Span::styled(path.clone(), Style::new().bold())));
            if e.kind == NodeKind::Link {
                lines.push(Line::from(Span::styled(
                    format!(
                        "Link to {}",
                        e.target.as_deref().unwrap_or("an unknown target")
                    ),
                    muted().italic(),
                )));
                lines.push(Line::from(
                    "What it points to is stored (and counted) there, not here. ⏎ follows the link.",
                ));
                f.render_widget(
                    Paragraph::new(lines).wrap(Wrap { trim: true }).block(block),
                    area,
                );
                return;
            }
            lines.push(Line::from(Span::styled(facts, muted())));
            match app.coverage(&path) {
                Some(c) if c.direct => lines.extend(finding_lines(c.finding)),
                Some(c) => lines.push(Line::from(
                    [
                        vec![
                            Span::raw("Inside "),
                            Span::styled(c.finding.title.clone(), safety_style(c.finding.safety)),
                            Span::styled(format!(" ({}): ", c.finding.safety), muted()),
                        ],
                        rich(&c.finding.how, muted()),
                    ]
                    .concat(),
                )),
                None => lines.push(Line::from(Span::styled(
                    "No suggestion covers this item.",
                    muted(),
                ))),
            }
        }
        None => lines.push(Line::from(Span::styled(
            format!(
                "{} files and {} folders, each smaller than {}.",
                e.files,
                e.dirs,
                human_bytes(app.snap.min_node_size)
            ),
            muted(),
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
        .enumerate()
        .map(|(i, fd)| {
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
                Cell::from(Line::from(vec![
                    Span::styled("● ", safety_style(fd.safety)),
                    Span::styled(fd.safety.to_string(), safety_style(fd.safety)),
                ])),
                Cell::from(fd.title.clone()),
                Cell::from(Span::styled(fd.action.to_string(), muted())),
                Cell::from(Span::styled(location, muted())),
            ])
            .style(if i == app.suggestion {
                th().selected()
            } else {
                Style::new()
            })
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(36),
            Constraint::Length(20),
            Constraint::Fill(1),
        ],
    );
    let mut state = TableState::default().with_selected(Some(app.suggestion));
    f.render_stateful_widget(selection_table(table), area, &mut state);
}

fn draw_suggestion_details(f: &mut Frame, app: &App, area: Rect) {
    let block = section("Details");
    let Some(fd) = app.selected_finding() else {
        f.render_widget(Paragraph::new(" No suggestions.").block(block), area);
        return;
    };
    let mut lines = finding_lines(fd);
    for item in fd.items.iter().take(3) {
        let from = item
            .source
            .as_ref()
            .map(|h| format!("  (from {h})"))
            .unwrap_or_default();
        lines.push(Line::from(Span::styled(
            format!(
                "  {:>9}  {}{from}",
                human_bytes(item.bytes),
                tilde(&item.path)
            ),
            muted(),
        )));
    }
    if fd.items.len() > 3 {
        lines.push(Line::from(Span::styled(
            format!("  … and {} more", fd.items.len() - 3),
            muted(),
        )));
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
        Some(s) => Line::from(Span::styled(format!(" {s}"), Style::new().fg(th().caution))),
        None => Line::from(match app.tab {
            Tab::Browse => key_hints(&[
                ("↑↓", "move"),
                ("⏎", "open"),
                ("←", "up"),
                ("d", "trash"),
                ("s", "sort"),
                ("r", "rescan"),
                ("?", "help"),
                ("q", "quit"),
            ]),
            Tab::Suggestions => key_hints(&[
                ("↑↓", "move"),
                ("⏎", "show in browser"),
                ("d", "trash all"),
                ("r", "rescan"),
                ("?", "help"),
                ("q", "quit"),
            ]),
        }),
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

fn key_pill(key: &str, bg: ratatui::style::Color) -> Span<'static> {
    Span::styled(format!(" {key} "), th().filled(bg))
}

fn draw_modal(f: &mut Frame, modal: &Modal) {
    let (title, text, border) = match modal {
        Modal::Help => {
            let row = |keys: &str, what: &str| {
                Line::from(vec![
                    Span::styled(format!("{keys:<16}"), accent().bold()),
                    Span::raw(what.to_string()),
                ])
            };
            (
                "Keys".to_string(),
                Text::from(vec![
                    row("tab  1  2", "switch between Suggestions and Browse"),
                    row("↑ ↓  j k", "move (PgUp / PgDn move faster)"),
                    row("⏎ → l", "open folder · show suggestion in the browser"),
                    row("← h ⌫", "up one folder"),
                    row("d", "move to Trash (asks first, never deletes outright)"),
                    row("s", "sort: size → name → oldest first"),
                    row("r", "rescan"),
                    row("q esc", "quit"),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled("● safe", safety_style(Safety::Safe)),
                        Span::styled(" nothing is lost    ", muted()),
                        Span::styled("● caution", safety_style(Safety::Caution)),
                        Span::styled(" safe after a quick step    ", muted()),
                        Span::styled("● review", safety_style(Safety::Review)),
                        Span::styled(" your call", muted()),
                    ]),
                ]),
                th().accent,
            )
        }
        Modal::Message { title, body } => (
            title.clone(),
            Text::from(
                body.lines()
                    .map(|l| Line::from(rich(l, Style::new())))
                    .collect::<Vec<_>>(),
            ),
            th().accent,
        ),
        Modal::ConfirmTrash { targets, warning } => {
            let total: u64 = targets.iter().map(|t| t.bytes).sum();
            let mut lines = Vec::new();
            for t in targets.iter().take(8) {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:>9}  ", human_bytes(t.bytes)),
                        Style::new().bold(),
                    ),
                    Span::raw(t.path.clone()),
                ]));
            }
            if targets.len() > 8 {
                lines.push(Line::from(Span::styled(
                    format!("           … and {} more", targets.len() - 8),
                    muted(),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("{} in total", human_bytes(total)),
                Style::new().bold(),
            )));
            if let Some(w) = warning {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    w.clone(),
                    Style::new().fg(th().caution),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                key_pill("y", th().caution),
                Span::raw(" move to Trash     "),
                key_pill("n", th().muted),
                Span::raw(" cancel"),
            ]));
            (
                "Move to Trash?".to_string(),
                Text::from(lines),
                th().caution,
            )
        }
    };
    // Size the dialog to its wrapped text, so the last line (often the y/n
    // prompt) is never cut off.
    let width = 90.min(f.area().width);
    let body = Paragraph::new(text).wrap(Wrap { trim: false });
    let height = body.line_count(width.saturating_sub(2)) as u16 + 2;
    let area = centered(f.area(), width, height);
    f.render_widget(Clear, area);
    f.render_widget(
        body.block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(border))
                .title(Span::styled(
                    format!(" {title} "),
                    Style::new().fg(border).bold(),
                )),
        ),
        area,
    );
}

pub fn draw_scanning(f: &mut Frame, root: &str, p: &ScanProgress, elapsed: Duration) {
    use std::sync::atomic::Ordering::Relaxed;
    let text = Text::from(vec![
        Line::from(vec![
            Span::styled("diskwise", accent().bold()),
            Span::raw("  scanning"),
        ]),
        Line::from(Span::styled(root.to_string(), Style::new().bold())),
        Line::from(""),
        Line::from(format!(
            "{} files · {} folders · {}",
            p.files.load(Relaxed),
            p.dirs.load(Relaxed),
            human_bytes(p.bytes.load(Relaxed))
        )),
        Line::from(Span::styled(
            format!("{:.0}s · q to quit", elapsed.as_secs_f64()),
            muted(),
        )),
    ]);
    let area = centered(f.area(), 60, 7);
    f.render_widget(
        Paragraph::new(text).centered().block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(th().rule)),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Every cell as `[symbol, fg, bg, bold]`, for turning screens into images
    /// when checking colours (see `render_real_snapshot`).
    fn cells_json(app: &App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        let buf = t.backend().buffer();
        let rows: Vec<String> = (0..h)
            .map(|y| {
                let cells: Vec<String> = (0..w)
                    .map(|x| {
                        let c = &buf[(x, y)];
                        format!(
                            "[{:?},\"{:?}\",\"{:?}\",{},{}]",
                            c.symbol(),
                            c.fg,
                            c.bg,
                            c.modifier.contains(ratatui::style::Modifier::BOLD),
                            c.modifier.contains(ratatui::style::Modifier::REVERSED)
                        )
                    })
                    .collect();
                format!("[{}]", cells.join(","))
            })
            .collect();
        format!("[{}]", rows.join(","))
    }

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

    /// Visual check against a real snapshot (optionally DISKWISE_COLOR=16 etc., and
    /// DISKWISE_RENDER_DIR to also write each screen's cells as JSON):
    /// DISKWISE_SNAPSHOT=snap.json cargo test -p diskwise-cli render_real -- --ignored --nocapture
    #[test]
    #[ignore]
    fn render_real_snapshot() {
        if let Ok(c) = std::env::var("DISKWISE_COLOR") {
            use clap::ValueEnum;
            super::super::theme::init(super::super::theme::ColorMode::from_str(&c, true).unwrap());
        }
        let path = std::env::var("DISKWISE_SNAPSHOT").expect("set DISKWISE_SNAPSHOT");
        let snap = crate::load_snapshot(std::path::Path::new(&path)).unwrap();
        let rules = crate::load_rules(snap.platform(), None).unwrap();
        let findings = diskwise_core::analyze(&snap, &rules, crate::now());
        let mut app = App::new(snap, findings);
        let home = std::env::var("HOME").unwrap();
        let out = std::env::var("DISKWISE_RENDER_DIR").ok();
        let show = |name: &str, app: &App| {
            println!("{}\n", render(app, 120, 32));
            if let Some(dir) = &out {
                std::fs::write(format!("{dir}/{name}.json"), cells_json(app, 120, 32)).unwrap();
            }
        };
        app.reveal(&format!(
            "{home}/Library/Application Support/Google/GoogleUpdater"
        ));
        show("browse", &app);
        app.toggle_tab();
        show("suggestions", &app);
        app.reveal(&format!("{home}/Library/Caches"));
        app.open();
        app.request_trash();
        show("confirm", &app);
    }

    #[test]
    fn dialog_never_cuts_off_the_prompt() {
        let mut app = super::super::app::sample_app("/r");
        let long = format!("/r/{}", "very-long-folder-name/".repeat(8));
        app.modal = Some(Modal::ConfirmTrash {
            targets: vec![super::super::app::TrashTarget {
                path: long,
                bytes: 1,
            }],
            warning: Some("A long caution that wraps. ".repeat(10)),
        });
        let screen = render(&app, 100, 40);
        assert!(
            screen.contains("move to Trash") && screen.contains("cancel"),
            "{screen}"
        );
    }

    #[test]
    fn commands_drop_backticks_and_stand_out() {
        let spans = rich("Run `sudo apt clean` now.", Style::new());
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Run sudo apt clean now.");
        assert_eq!(spans[1].content, "sudo apt clean");
        assert!(
            spans[1]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
    }

    #[test]
    fn renders_browser_and_suggestions() {
        let (mut app, r, dir) = super::super::app::disk_app("render");
        app.reveal(&format!("{r}/home/v/cache"));
        let screen = render(&app, 180, 30);
        assert!(screen.contains(&format!("{r}/home/v")), "{screen}");
        assert!(
            screen.contains("cache/") && screen.contains("● cache"),
            "{screen}"
        );
        app.toggle_tab();
        let screen = render(&app, 180, 30);
        assert!(
            screen.contains("Suggestions · 3") && screen.contains("safe"),
            "{screen}"
        );
        app.request_trash();
        let screen = render(&app, 180, 30);
        assert!(
            screen.contains("Move to Trash?") && screen.contains(&format!("{r}/home/v/cache")),
            "{screen}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
