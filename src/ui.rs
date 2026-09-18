//! Pure terminal projection. It never owns selection or filesystem paths.
use crate::app::App;
use crate::model::*;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

const ACCENT: Color = Color::Cyan;
const HELP_BACKGROUND: Color = Color::Rgb(24, 29, 36);
const HELP_TEXT: Color = Color::Rgb(245, 247, 250);
const HELP_KEY: Color = Color::Rgb(128, 216, 255);

/// A selectable list in the last rendered frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseList {
    Column(usize),
    Matches,
}

#[derive(Default)]
pub struct HitMap {
    regions: Vec<(Rect, MouseList, Vec<NodeKey>)>,
}

impl HitMap {
    /// Returns the list and rendered row identity, or an empty list-space hit.
    pub fn hit(&self, x: u16, y: u16) -> Option<(MouseList, Option<&NodeKey>)> {
        self.regions.iter().find_map(|(area, list, rows)| {
            (x >= area.x && x < area.right() && y >= area.y && y < area.bottom())
                .then(|| (*list, rows.get((y - area.y) as usize)))
        })
    }
}

fn shortcut_help(width: u16, height: u16) -> Vec<Line<'static>> {
    let groups: &[&[(&str, &str)]] = &[
        &[
            ("↑↓", "select"),
            ("←→", "columns"),
            ("Enter", "cd"),
            ("Ctrl+O", "hd"),
            ("Ctrl+P", "pane"),
            ("Ctrl+C", "exit"),
        ],
        &[
            ("Ctrl+A", "add root"),
            ("Ctrl+N", "mkdir"),
            ("Ctrl+G", "git init"),
            ("Ctrl+R", "refresh branch"),
            ("Ctrl+F", "reload roots"),
            ("Ctrl+L", "branch"),
        ],
    ];
    let key_style = Style::default().fg(HELP_KEY).add_modifier(Modifier::BOLD);
    let available = width.saturating_sub(2) as usize;
    let mut lines = Vec::new();
    for group in groups {
        let mut spans = vec![Span::raw(" ")];
        let mut used = 0;
        for &(key, label) in *group {
            let pair_width = key.width() + 1 + label.width();
            if used > 0 && used + 2 + pair_width > available {
                lines.push(Line::from(spans));
                spans = vec![Span::raw(" ")];
                used = 0;
            }
            if used > 0 {
                spans.push(Span::raw("  "));
                used += 2;
            }
            spans.push(Span::styled(key, key_style));
            spans.push(Span::raw(" "));
            spans.push(Span::raw(label));
            used += pair_width;
        }
        lines.push(Line::from(spans));
    }
    // Retain room for a selectable directory, target, and status even at 24×8.
    if lines.len() > height.saturating_sub(6) as usize {
        return vec![
            Line::from(vec![
                Span::raw(" "),
                Span::styled("Ctrl+C", key_style),
                Span::raw(" exit"),
            ]),
            Line::from(" Resize for shortcuts"),
        ];
    }
    lines
}

pub fn draw(frame: &mut Frame, app: &App) -> HitMap {
    let mut hits = HitMap::default();
    let area = frame.area();
    if area.width < 24 || area.height < 8 {
        frame.render_widget(
            Paragraph::new("qnav: window too small\nResize to at least 24x8\nCtrl-C exit"),
            area,
        );
        return hits;
    }
    let target = app
        .target()
        .and_then(|k| app.path(&k))
        .map(|p| display_path(&p))
        .unwrap_or_else(|| "(none — select a directory)".into());
    let target_paragraph = Paragraph::new(format!("Target: {target}")).wrap(Wrap { trim: false });
    let target_lines = target_paragraph.line_count(area.width).max(1);
    let help = shortcut_help(area.width, area.height);
    let help_height = help.len() as u16;
    let target_room = area.height.saturating_sub(help_height + 5).max(1);
    let target_height = target_lines
        .min((area.height as usize / 3).max(1))
        .min(target_room as usize) as u16;
    let regions = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(target_height),
            Constraint::Length(1),
            Constraint::Length(help_height),
        ])
        .split(area);
    let header = Line::from(vec![
        Span::styled(
            " qnav ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  depth {} / 9  [− ]+   ", app.depth)),
        Span::styled(
            if app.query().is_empty() {
                "type to filter".into()
            } else {
                format!("filter: {}", display_os(app.query().as_ref()))
            },
            Style::default().fg(ACCENT),
        ),
    ]);
    frame.render_widget(Paragraph::new(header), regions[0]);
    let browse = app.browse_projection();
    if let Navigation::Filter(s) = &app.navigation {
        let parts = Layout::vertical([Constraint::Percentage(52), Constraint::Percentage(48)])
            .split(regions[1]);
        draw_columns(frame, app, &browse, parts[0], s.viewport, &mut hits);
        let selected = match &s.selection {
            MatchSelection::Selected(k) => s.matches.iter().position(|m| m == k),
            _ => None,
        };
        let height = parts[1].height.saturating_sub(2) as usize;
        let offset = selected
            .unwrap_or(0)
            .saturating_sub(height.saturating_sub(1) / 2)
            .min(s.matches.len().saturating_sub(height));
        hits.regions.push((
            Block::default().borders(Borders::ALL).inner(parts[1]),
            MouseList::Matches,
            s.matches
                .iter()
                .skip(offset)
                .take(height)
                .cloned()
                .collect(),
        ));
        let rows: Vec<_> = s
            .matches
            .iter()
            .skip(offset)
            .take(height)
            .map(|key| {
                let root = app.roots.iter().find(|r| r.id == key.root).unwrap();
                let label = format!(
                    "{} / {}",
                    display_path(&root.path),
                    display_path(&key.relative)
                );
                ListItem::new(label)
            })
            .collect();
        let title = format!(
            " Matches {} · {} · Tab browse / Esc restore ",
            s.matches.len(),
            app.completion()
        );
        if rows.is_empty() {
            let text = if app.completion() == "partial" {
                "No matches yet — search incomplete"
            } else if app.completion() == "complete" {
                "No matches"
            } else {
                "Searching allowed roots…"
            };
            frame.render_widget(
                Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(title)),
                parts[1],
            );
        } else {
            let list = List::new(rows)
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_symbol("> ")
                .highlight_style(Style::default().fg(ACCENT));
            frame.render_stateful_widget(
                list,
                parts[1],
                &mut ListState::default().with_selected(selected.map(|i| i - offset)),
            );
        }
    } else {
        draw_columns(frame, app, &browse, regions[1], 0, &mut hits);
    }
    let target_paragraph = if target_lines > target_height as usize {
        Paragraph::new(format!("Resize to read full target:\n{target}")).wrap(Wrap { trim: false })
    } else {
        target_paragraph
    };
    frame.render_widget(
        target_paragraph.style(Style::default().fg(ACCENT)),
        regions[2],
    );
    let status = if app.status.is_empty() {
        app.branch()
            .map(|p| format!("Ctrl-R refresh branch: {}", display_path(&p)))
            .unwrap_or_else(|| "Only manual roots · Ctrl-A to add a directory".into())
    } else {
        display_os(app.status.as_ref())
    };
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::Yellow)),
        regions[3],
    );
    frame.render_widget(
        Paragraph::new(help).style(
            Style::default()
                .fg(HELP_TEXT)
                .bg(HELP_BACKGROUND)
                .add_modifier(Modifier::BOLD),
        ),
        regions[4],
    );
    if let Some(input) = &app.add_root {
        draw_form(
            frame,
            "Add existing directory",
            None,
            Some(input),
            "Enter save · Esc cancel · relative paths use startup cwd",
        );
    }
    if let Some(action) = &app.pending_action {
        let title = match action.kind {
            ActionKind::ChangeDirectory => "Change directory",
            ActionKind::Mkdir => "Create child directory",
            ActionKind::GitInit => "Git init",
            ActionKind::OpenHd => "Open hd",
            ActionKind::OpenPane => "Open shell pane",
            ActionKind::ListBranches => "List branches",
            ActionKind::OpenBranchWorkspace => "Open branch workspace",
        };
        match &action.phase {
            ActionPhase::Input(input) => draw_form(
                frame,
                title,
                Some(&action.target),
                Some(input),
                "Enter create · Esc cancel · one child name",
            ),
            ActionPhase::Confirm { message, accepted } => draw_form(
                frame,
                title,
                Some(&action.target),
                None,
                &format!(
                    "{message}\n{} Cancel   {} Continue\n←→ select · Enter confirm · Esc cancel",
                    if *accepted { "[ ]" } else { "[x]" },
                    if *accepted { "[x]" } else { "[ ]" }
                ),
            ),
            ActionPhase::Checking => {}
            ActionPhase::Running => {}
            ActionPhase::Cancelling => {}
        }
    }
    if let Some(picker) = &app.branch_picker {
        draw_branch_picker(frame, picker);
        hits.regions.clear();
    }
    if app.add_root.is_some()
        || app.pending_action.as_ref().is_some_and(|action| {
            matches!(
                action.phase,
                ActionPhase::Input(_) | ActionPhase::Confirm { .. }
            )
        })
    {
        hits.regions.clear();
    }
    hits
}

fn draw_branch_picker(frame: &mut Frame, picker: &BranchPicker) {
    let screen = frame.area();
    let width = screen.width.saturating_sub(4).clamp(20, 100);
    let height = screen.height.saturating_sub(4).clamp(6, 24);
    let rect = Rect::new(
        screen.x + screen.width.saturating_sub(width) / 2,
        screen.y + screen.height.saturating_sub(height) / 2,
        width.min(screen.width),
        height.min(screen.height),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Branch workspace ")
        .border_style(Style::default().fg(ACCENT));
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner);
    frame.render_widget(
        Paragraph::new(format!("Filter: {}▏", display_os(picker.query.as_ref()))),
        parts[0],
    );
    if picker.phase == BranchPickerPhase::Loading {
        frame.render_widget(Paragraph::new("Loading branches…  Esc cancel"), parts[1]);
        return;
    }
    let visible = picker.visible_indices();
    if visible.is_empty() {
        frame.render_widget(Paragraph::new("No matching branches\nEsc cancel"), parts[1]);
        return;
    }
    let selected = picker.selected.min(visible.len().saturating_sub(1));
    let height = parts[1].height as usize;
    let offset = selected
        .saturating_sub(height.saturating_sub(1) / 2)
        .min(visible.len().saturating_sub(height));
    let rows = visible
        .iter()
        .skip(offset)
        .take(height)
        .map(|index| {
            let branch = &picker.branches[*index];
            let state = if branch.stale_worktree {
                "stale worktree metadata".into()
            } else {
                branch
                    .worktree
                    .as_ref()
                    .map(|path| format!("worktree {}", display_path(path)))
                    .unwrap_or_else(|| "new worktree".into())
            };
            ListItem::new(format!(
                "{}  · {state}",
                display_os(branch.name.as_os_str())
            ))
        })
        .collect::<Vec<_>>();
    let list = List::new(rows)
        .highlight_symbol("> ")
        .highlight_style(Style::default().fg(ACCENT));
    frame.render_stateful_widget(
        list,
        parts[1],
        &mut ListState::default().with_selected(Some(selected - offset)),
    );
}

fn draw_columns(
    frame: &mut Frame,
    app: &App,
    browse: &BrowseState,
    area: Rect,
    manual_offset: usize,
    hits: &mut HitMap,
) {
    let count = (browse.chain.len() + 1).min(app.depth + 1).max(1);
    let visible = (area.width as usize / 26).max(1).min(count);
    let focus = browse.focus.min(count - 1);
    let start = if matches!(app.navigation, Navigation::Filter(_)) {
        manual_offset.min(count.saturating_sub(visible))
    } else {
        focus.saturating_sub(visible - 1)
    };
    let widths = vec![Constraint::Ratio(1, visible as u32); visible];
    let columns = Layout::horizontal(widths).split(area);
    for (view_index, column_area) in columns.iter().enumerate() {
        let column = start + view_index;
        let active = column == focus
            && app.target().is_some()
            && !matches!(app.navigation, Navigation::Filter(_));
        let title = if column == 0 {
            "Roots".into()
        } else {
            browse
                .chain
                .get(column - 1)
                .and_then(|k| app.path(k))
                .map(|p| display_os(p.file_name().unwrap_or(p.as_os_str())))
                .unwrap_or_default()
        };
        let title = format!(
            " {}{} · {} ",
            if start > 0 && view_index == 0 {
                "‹ "
            } else {
                ""
            },
            title,
            column
        );
        let rows = app.column_rows(column, browse);
        let mut block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(if column == focus {
                ACCENT
            } else {
                Color::DarkGray
            }));
        if let Some(node) = column
            .checked_sub(1)
            .and_then(|i| browse.chain.get(i))
            .and_then(|k| app.nodes.get(k))
        {
            let state = match &node.listing {
                ListingState::Unread | ListingState::Loading(_) => Some(" Loading… ".into()),
                ListingState::Partial(reason) => {
                    Some(format!(" Partial: {} ", display_os(reason.as_ref())))
                }
                ListingState::Failed(reason) => {
                    Some(format!(" Unavailable: {} ", display_os(reason.as_ref())))
                }
                ListingState::Ready => None,
            };
            if let Some(state) = state {
                block =
                    block.title_bottom(Line::from(state).style(Style::default().fg(Color::Yellow)));
            }
        }
        let selected = browse
            .chain
            .get(column)
            .and_then(|k| rows.iter().position(|r| r == k));
        let height = column_area.height.saturating_sub(2) as usize;
        let offset = selected
            .unwrap_or(0)
            .saturating_sub(height.saturating_sub(1) / 2)
            .min(rows.len().saturating_sub(height));
        if matches!(app.navigation, Navigation::Browse(_)) {
            hits.regions.push((
                block.inner(*column_area),
                MouseList::Column(column),
                rows.iter().skip(offset).take(height).cloned().collect(),
            ));
        }
        if rows.is_empty() {
            let text = if column == 0 {
                "Ctrl-A\nAdd a directory".into()
            } else {
                browse
                    .chain
                    .get(column - 1)
                    .and_then(|k| app.nodes.get(k))
                    .map(|n| match &n.listing {
                        ListingState::Unread | ListingState::Loading(_) => "Loading…".into(),
                        ListingState::Ready => "(empty)".into(),
                        ListingState::Partial(e) => format!("Partial: {e}"),
                        ListingState::Failed(e) => format!("Unavailable: {e}"),
                    })
                    .unwrap_or_default()
            };
            frame.render_widget(
                Paragraph::new(display_os(text.as_ref()))
                    .block(block)
                    .wrap(Wrap { trim: false }),
                *column_area,
            );
            continue;
        }
        let items = rows
            .iter()
            .skip(offset)
            .take(height)
            .map(|k| {
                let p = app.path(k).unwrap();
                let name = display_os(p.file_name().unwrap_or(p.as_os_str()));
                let failed = app
                    .nodes
                    .get(k)
                    .is_some_and(|n| matches!(n.listing, ListingState::Failed(_)));
                ListItem::new(if failed { format!("! {name}") } else { name })
            })
            .collect::<Vec<_>>();
        let list = List::new(items)
            .block(block)
            .highlight_symbol(if active { "> " } else { "· " })
            .highlight_style(Style::default().fg(if active { ACCENT } else { Color::Gray }));
        frame.render_stateful_widget(
            list,
            *column_area,
            &mut ListState::default().with_selected(selected.map(|i| i - offset)),
        );
    }
}

fn draw_form(
    frame: &mut Frame,
    title: &str,
    path: Option<&std::path::Path>,
    input: Option<&str>,
    hint: &str,
) {
    let screen = frame.area();
    let width = screen.width.saturating_sub(4).clamp(20, 100);
    let text = format!(
        "{}{}{}\n\n{}",
        path.map(|p| format!("Target: {}\n\n", display_path(p)))
            .unwrap_or_default(),
        input
            .map(|s| format!("> {}", display_os(s.as_ref())))
            .unwrap_or_default(),
        if input.is_some() { "▏" } else { "" },
        hint
    );
    let lines = text
        .lines()
        .map(|l| l.width().max(1).div_ceil(width.saturating_sub(4) as usize))
        .sum::<usize>();
    let height = (lines as u16 + 4)
        .min(screen.height.saturating_sub(2))
        .max(4);
    let rect = Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {title} "))
                    .border_style(Style::default().fg(ACCENT)),
            )
            .wrap(Wrap { trim: false }),
        rect,
    );
}
