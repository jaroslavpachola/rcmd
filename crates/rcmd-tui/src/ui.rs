use chrono::{DateTime, Local};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Row, Table, TableState};
use ratatui::Frame;
use rcmd_core::entry::{Entry, EntryKind};
use rcmd_core::panel::Panel;

use crate::app::App;

const PANEL_BG: Color = Color::Blue;
const PANEL_FG: Color = Color::Gray;
const DIR_FG: Color = Color::White;
const BROKEN_FG: Color = Color::LightRed;
const HEADER_FG: Color = Color::Yellow;
const SELECT_BG: Color = Color::Cyan;
const SELECT_FG: Color = Color::Black;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [main, status, keybar] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(main);

    // 2 border rows + 1 column-header row.
    app.panel_rows = main.height.saturating_sub(3) as usize;

    draw_panel(
        frame,
        left,
        &app.panels[0],
        &mut app.table_states[0],
        app.active == 0,
    );
    draw_panel(
        frame,
        right,
        &app.panels[1],
        &mut app.table_states[1],
        app.active == 1,
    );
    draw_status(frame, status, app);
    draw_keybar(frame, keybar);
}

fn draw_panel(frame: &mut Frame, area: Rect, panel: &Panel, state: &mut TableState, active: bool) {
    let title_style = if active {
        Style::new().fg(SELECT_FG).bg(SELECT_BG)
    } else {
        Style::new().fg(PANEL_FG).bg(PANEL_BG)
    };
    let block = Block::bordered()
        .style(Style::new().fg(PANEL_FG).bg(PANEL_BG))
        .title(Span::styled(
            format!(" {} ", panel.cwd.display()),
            title_style,
        ));

    let header = Row::new([
        Cell::from(Line::from("Name").centered()),
        Cell::from(Line::from("Size").centered()),
        Cell::from(Line::from("Modify time").centered()),
    ])
    .style(Style::new().fg(HEADER_FG));

    let rows = panel.entries.iter().map(entry_row);

    let table = Table::new(
        rows,
        [
            Constraint::Fill(1),
            Constraint::Length(7),
            Constraint::Length(12),
        ],
    )
    .header(header)
    .column_spacing(1)
    .style(Style::new().fg(PANEL_FG).bg(PANEL_BG))
    .row_highlight_style(if active {
        Style::new().fg(SELECT_FG).bg(SELECT_BG)
    } else {
        Style::new()
    })
    .block(block);

    state.select(Some(panel.cursor));
    frame.render_stateful_widget(table, area, state);
}

fn entry_row(entry: &Entry) -> Row<'_> {
    let (marker, style) = match entry.kind {
        EntryKind::Dir => ("/", Style::new().fg(DIR_FG).add_modifier(Modifier::BOLD)),
        EntryKind::SymlinkDir => ("~", Style::new().fg(DIR_FG).add_modifier(Modifier::BOLD)),
        EntryKind::SymlinkFile => ("@", Style::new().fg(PANEL_FG)),
        EntryKind::SymlinkBroken => ("!", Style::new().fg(BROKEN_FG)),
        EntryKind::File => (" ", Style::new().fg(PANEL_FG)),
    };

    let size = if entry.is_parent() {
        "UP--DIR".to_string()
    } else {
        format_size(entry.size)
    };
    let mtime = entry
        .mtime
        .map(|t| DateTime::<Local>::from(t).format("%b %e %H:%M").to_string())
        .unwrap_or_default();

    Row::new([
        Cell::from(format!("{marker}{}", entry.name.to_string_lossy())),
        Cell::from(Line::from(size).right_aligned()),
        Cell::from(mtime),
    ])
    .style(style)
}

/// Fits in 7 columns: plain bytes below 10M, then K/M/G like MC.
fn format_size(size: u64) -> String {
    if size < 10_000_000 {
        return size.to_string();
    }
    let kb = size / 1024;
    if kb < 10_000_000 {
        return format!("{kb}K");
    }
    let mb = kb / 1024;
    if mb < 10_000_000 {
        return format!("{mb}M");
    }
    format!("{}G", mb / 1024)
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let line = if let Some(msg) = &app.status {
        Line::from(msg.as_str()).style(Style::new().fg(Color::White).bg(Color::Red))
    } else {
        let selected = app.panels[app.active]
            .selected()
            .map(|e| e.name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Line::from(selected).style(Style::new().fg(PANEL_FG))
    };
    frame.render_widget(line, area);
}

const KEYBAR: [(&str, &str); 10] = [
    ("1", "Help"),
    ("2", "Menu"),
    ("3", "View"),
    ("4", "Edit"),
    ("5", "Copy"),
    ("6", "RenMov"),
    ("7", "Mkdir"),
    ("8", "Delete"),
    ("9", "PullDn"),
    ("10", "Quit"),
];

fn draw_keybar(frame: &mut Frame, area: Rect) {
    let mut spans = Vec::with_capacity(KEYBAR.len() * 2);
    for (num, label) in KEYBAR {
        spans.push(Span::styled(
            format!("{num:>2}"),
            Style::new().fg(Color::White).bg(Color::Black),
        ));
        spans.push(Span::styled(
            format!("{label:<6}"),
            Style::new().fg(Color::Black).bg(Color::Cyan),
        ));
    }
    frame.render_widget(Line::from(spans), area);
}
