use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line as ALine};
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::vte::ansi::{Color as AColor, NamedColor};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, AppMode, RootView};
use crate::keymap::{NormalAction, ViewAction};
use crate::pty_renderer::PtyRenderer;
use clhorde_core::control::{DetailNode, WorkflowSummary};
use clhorde_core::prompt::{PromptMode, PromptStatus};

pub fn render(f: &mut Frame, app: &mut App) {
    // Drafts and Workflows tabs don't have an input bar — they're
    // navigation views with no text composition. Reclaim the space so
    // the list fills the panel.
    let on_prompts_tab = app.root_view == RootView::Prompts;
    let input_bar_height = if !on_prompts_tab {
        0
    } else if app.mode == AppMode::Insert && app.input.is_multiline() {
        (app.input.line_count() as u16 + 2).clamp(3, 10) // +2 for borders
    } else {
        3
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),                // status bar (1 content + bottom border)
            Constraint::Length(1),                // tab bar (Phase 4)
            Constraint::Min(5),                   // main area
            Constraint::Length(input_bar_height), // input bar
            Constraint::Length(1),                // help bar
        ])
        .split(f.area());

    render_status_bar(f, app, chunks[0]);
    render_tab_bar(f, app, chunks[1]);
    render_main_area_dispatch(f, app, chunks[2]);
    if on_prompts_tab {
        render_input_bar(f, app, chunks[3]);
        render_suggestions(f, app, chunks[3]);
        render_template_suggestions(f, app, chunks[3]);
    }
    render_help_bar(f, app, chunks[4]);

    if app.show_quick_prompts_popup
        && on_prompts_tab
        && (app.mode == AppMode::ViewOutput || app.mode == AppMode::PtyInteract)
    {
        render_quick_prompts_popup(f, app, chunks[2]);
    }

    if app.confirm_quit {
        render_quit_confirmation(f, f.area());
    }

    if app.confirm_batch_delete {
        render_batch_delete_confirmation(f, app, f.area());
    }

    if app.show_help_overlay {
        render_help_overlay(f, app, f.area());
    }

    if app.retry_section_input.is_some() {
        render_retry_section_prompt(f, app, f.area());
    }
}

/// Centered popup for the inline retry-section prompt fired by `T` on
/// the Workflows tab. Captures dotted decimal section ids; Enter
/// submits, Esc cancels.
fn render_retry_section_prompt(f: &mut Frame, app: &App, area: Rect) {
    let Some(input) = app.retry_section_input.as_ref() else {
        return;
    };
    let popup_w = 50.min(area.width.saturating_sub(4));
    let popup_h = 7.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect::new(x, y, popup_w, popup_h);

    f.render_widget(Clear, popup);
    let block = Block::default()
        .title(" Retry section ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let body = vec![
        Line::from(vec![
            Span::styled(
                "  workflow: ",
                Style::default().fg(Color::Gray),
            ),
            Span::styled(
                input.workflow.as_str(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                "  section:  ",
                Style::default().fg(Color::Gray),
            ),
            Span::styled(
                if input.buffer.is_empty() {
                    "_"
                } else {
                    input.buffer.as_str()
                },
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  Enter to submit, Esc to cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(Paragraph::new(body).block(block), popup);
}

fn render_main_area_dispatch(f: &mut Frame, app: &mut App, area: Rect) {
    // The detail overlay lives inside the Workflows tab — render it
    // instead of the list whenever it is open. We keep the same area
    // so the tab bar / status line don't shift around.
    if app.root_view == RootView::Workflows && app.workflow_detail.is_some() {
        render_workflow_detail_view(f, app, area);
        return;
    }
    match app.root_view {
        RootView::Prompts => render_main_area(f, app, area),
        RootView::Drafts => render_drafts_view(f, app, area),
        RootView::Workflows => render_workflows_view(f, app, area),
    }
}

fn render_workflow_detail_view(f: &mut Frame, app: &App, area: Rect) {
    let Some(detail) = app.workflow_detail.as_ref() else {
        return;
    };
    let (color, label) = workflow_status_style(&detail.status);
    let title = format!(
        " {} · {label}{} ",
        detail.name,
        freshness_suffix(app.detail_last_refresh_age_secs())
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color));

    let mut lines: Vec<Line> = Vec::new();

    // ── Header (priority + timestamps + failure reason) ──
    let mut header_spans: Vec<Span> = Vec::new();
    if detail.priority != 0 {
        header_spans.push(Span::styled(
            format!("priority={}  ", detail.priority),
            Style::default().fg(Color::Gray),
        ));
    }
    if let Some(t) = detail.queued_at {
        header_spans.push(Span::styled(
            format!("queued {}  ", t.format("%H:%M:%S")),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(t) = detail.started_at {
        header_spans.push(Span::styled(
            format!("started {}  ", t.format("%H:%M:%S")),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(t) = detail.finished_at {
        header_spans.push(Span::styled(
            format!("finished {}  ", t.format("%H:%M:%S")),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if !header_spans.is_empty() {
        lines.push(Line::from(header_spans));
    }
    if let Some(reason) = &detail.failure_reason {
        lines.push(Line::from(Span::styled(
            format!("  reason: {reason}"),
            Style::default().fg(Color::Red),
        )));
    }
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }

    // ── Apply phase (DAG nodes) ──
    lines.push(Line::from(Span::styled(
        "  Apply",
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
    )));
    if detail.apply.is_empty() {
        lines.push(Line::from(Span::styled(
            "    (no DAG yet — tasks.md not parsed)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for node in &detail.apply {
            lines.push(detail_node_line(node, "    "));
        }
    }
    lines.push(Line::from(""));

    // ── Verify phase ──
    lines.push(Line::from(Span::styled(
        "  Verify",
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
    )));
    if let Some(node) = &detail.verify {
        lines.push(detail_node_line(node, "    "));
    } else {
        lines.push(Line::from(Span::styled(
            "    (not dispatched)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));

    // ── Archive phase ──
    lines.push(Line::from(Span::styled(
        "  Archive",
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
    )));
    if let Some(node) = &detail.archive {
        lines.push(detail_node_line(node, "    "));
    } else {
        lines.push(Line::from(Span::styled(
            "    (not dispatched)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));

    // ── Footer hint ──
    lines.push(Line::from(Span::styled(
        "  Esc / Enter: back  ·  j/k: scroll  ·  r: refresh  ·  X cancel  T retry  R read",
        Style::default().fg(Color::DarkGray),
    )));

    // Clamp scroll so `G` doesn't overshoot.
    let max_scroll = lines.len().saturating_sub(1) as u16;
    let scroll = app.detail_scroll.min(max_scroll);

    let para = Paragraph::new(lines).block(block).scroll((scroll, 0));
    f.render_widget(para, area);
}

fn detail_node_line<'a>(node: &'a DetailNode, indent: &'a str) -> Line<'a> {
    let (icon, color) = detail_state_style(&node.state);
    let mut spans: Vec<Span> = vec![
        Span::raw(indent),
        Span::styled(icon, Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(
            format!("{:<6}", node.id),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(node.label.as_str(), Style::default().fg(Color::Gray)),
    ];
    if let Some(pid) = node.prompt_id {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("#{pid}"),
            Style::default().fg(Color::Cyan),
        ));
    }
    if let Some(code) = node.exit_code {
        let exit_color = if code == 0 { Color::Green } else { Color::Red };
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("exit={code}"),
            Style::default().fg(exit_color),
        ));
    }
    if !node.depends_on.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("← {}", node.depends_on.join(", ")),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

/// Format a "(updated Ns ago)" badge for titles. Empty when no
/// successful refresh has happened yet so the header doesn't lie.
/// 0–1s renders as "fresh" since the human eye can't tell the
/// difference and the constant churn is distracting.
fn freshness_suffix(age: Option<u64>) -> String {
    match age {
        None => String::new(),
        Some(0) | Some(1) => " · fresh".to_string(),
        Some(s) if s < 60 => format!(" · {s}s"),
        Some(s) if s < 3600 => format!(" · {}m", s / 60),
        Some(s) => format!(" · {}h", s / 3600),
    }
}

fn detail_state_style(state: &str) -> (&'static str, Color) {
    match state {
        "pending" => ("○", Color::DarkGray),
        "running" => ("▶", Color::Cyan),
        "completed" => ("✓", Color::Green),
        "failed" => ("✗", Color::Red),
        _ => ("?", Color::Gray),
    }
}

/// Top-of-screen tab bar: `[1] Prompts  [2] Drafts  [3] Workflows`.
/// The active tab is highlighted; the digit prefix doubles as a visible
/// reminder of the keybinding.
fn render_tab_bar(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::with_capacity(7);
    let pad = Span::raw("  ");
    for (i, (digit, label, view)) in [
        ('1', "Prompts", RootView::Prompts),
        ('2', "Drafts", RootView::Drafts),
        ('3', "Workflows", RootView::Workflows),
    ]
    .iter()
    .enumerate()
    {
        if i > 0 {
            spans.push(pad.clone());
        }
        let active = app.root_view == *view;
        let fg = if active { Color::Black } else { Color::Gray };
        let bg = if active {
            Color::Cyan
        } else {
            Color::Rgb(30, 30, 40)
        };
        let modifier = if active {
            Modifier::BOLD
        } else {
            Modifier::empty()
        };
        spans.push(Span::styled(
            format!(" [{digit}] {label} "),
            Style::default().fg(fg).bg(bg).add_modifier(modifier),
        ));
    }
    let para = Paragraph::new(Line::from(spans))
        .style(Style::default().bg(Color::Rgb(30, 30, 40)));
    f.render_widget(para, area);
}

/// Drafts tab: list of `openspec/changes/<X>/` directories without a
/// `.clhorde-ready` marker. Polled from the scheduler control socket.
fn render_drafts_view(f: &mut Frame, app: &App, area: Rect) {
    let title = format!(
        " Drafts ({}){} ",
        app.drafts.len(),
        freshness_suffix(app.scheduler_last_refresh_age_secs())
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    if !app.scheduler_reachable {
        let body = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  scheduler not reachable",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Start it with: clhorde-scheduler daemon",
                Style::default().fg(Color::Gray),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  (press 'r' to retry)",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        f.render_widget(Paragraph::new(body).block(block), area);
        return;
    }

    if app.drafts.is_empty() {
        let body = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  no drafts",
                Style::default().fg(Color::Gray),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Drafts appear when openspec/changes/<X>/ exists",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "  without a .clhorde-ready marker.",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        f.render_widget(Paragraph::new(body).block(block), area);
        return;
    }

    let items: Vec<ListItem> = app
        .drafts
        .iter()
        .map(|n| ListItem::new(Line::from(Span::raw(format!("  {n}")))))
        .collect();
    let mut state = ListState::default();
    if !app.drafts.is_empty() {
        state.select(Some(app.drafts_selected.min(app.drafts.len() - 1)));
    }
    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(50, 50, 80))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▌ ");
    f.render_stateful_widget(list, area, &mut state);
}

/// Workflows tab: queued + running + recent terminal workflows. One
/// line per workflow showing status and (if present) a short suffix.
fn render_workflows_view(f: &mut Frame, app: &App, area: Rect) {
    let title = format!(
        " Workflows ({}){} ",
        app.workflows.len(),
        freshness_suffix(app.scheduler_last_refresh_age_secs())
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    if !app.scheduler_reachable {
        let body = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  scheduler not reachable",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Start it with: clhorde-scheduler daemon",
                Style::default().fg(Color::Gray),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  (press 'r' to retry)",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        f.render_widget(Paragraph::new(body).block(block), area);
        return;
    }

    if app.workflows.is_empty() {
        let body = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  no workflows",
                Style::default().fg(Color::Gray),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  A workflow is created when a change is queued",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "  (writing openspec/changes/<X>/.clhorde-ready).",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        f.render_widget(Paragraph::new(body).block(block), area);
        return;
    }

    let items: Vec<ListItem> = app
        .workflows
        .iter()
        .map(|w| ListItem::new(workflow_line(w)))
        .collect();
    let mut state = ListState::default();
    if !app.workflows.is_empty() {
        state.select(Some(
            app.workflows_selected.min(app.workflows.len() - 1),
        ));
    }
    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(50, 50, 80))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▌ ");
    f.render_stateful_widget(list, area, &mut state);
}

fn workflow_line(w: &WorkflowSummary) -> Line<'static> {
    let (color, label) = workflow_status_style(&w.status);
    let mut spans: Vec<Span> = vec![
        Span::raw("  "),
        Span::styled(
            format!("{:<24}", w.name),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{label:<13}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(reason) = &w.failure_reason {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            reason.clone(),
            Style::default().fg(Color::Red),
        ));
    } else if w.priority != 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("p={}", w.priority),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

fn workflow_status_style(status: &str) -> (Color, &'static str) {
    match status {
        "drafted" => (Color::DarkGray, "drafted"),
        "queued" => (Color::Yellow, "queued"),
        "implementing" => (Color::Cyan, "implementing"),
        "verifying" => (Color::Blue, "verifying"),
        "archiving" => (Color::Magenta, "archiving"),
        "archived" => (Color::Green, "archived"),
        "cancelled" => (Color::DarkGray, "cancelled"),
        "failed" => (Color::Red, "failed"),
        _ => (Color::Gray, "?"),
    }
}

fn render_status_bar(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let (mode_str, mode_color) = match app.mode {
        AppMode::Normal => ("NORMAL", Color::Blue),
        AppMode::Insert => ("INSERT", Color::Green),
        AppMode::ViewOutput => ("VIEW", Color::Yellow),
        AppMode::Interact => ("INTERACT", Color::Magenta),
        AppMode::PtyInteract => ("PTY", Color::Green),
        AppMode::Filter => ("FILTER", Color::Cyan),
    };

    let sep = Span::styled(" │ ", Style::default().fg(Color::DarkGray));

    // --- Worker utilization progress bar ---
    let bar_width = app.max_workers.min(8); // cap visual width at 8
    let filled = if app.max_workers > 0 {
        (app.active_workers * bar_width).div_ceil(app.max_workers)
    } else {
        0
    };
    let empty = bar_width - filled;
    let bar_filled: String = "█".repeat(filled);
    let bar_empty: String = "░".repeat(empty);

    // --- Condensed counters ---
    let pending = app.pending_count();
    let done = app.completed_count();
    let total = app.prompts.len();

    // --- Selected prompt inline status ---
    let selected_info: Vec<Span> = if let Some(prompt) = app.selected_prompt() {
        let status = prompt.status_enum();
        let (status_char, status_color) = match status {
            PromptStatus::Pending => ("·", Color::DarkGray),
            PromptStatus::Blocked => ("⊘", Color::DarkGray),
            PromptStatus::Running => ("▶", Color::Cyan),
            PromptStatus::Idle => ("◆", Color::Magenta),
            PromptStatus::Completed => ("✓", Color::Green),
            PromptStatus::Failed => ("✗", Color::Red),
        };
        let mut parts = vec![
            sep.clone(),
            Span::styled(
                format!("#{}", prompt.id),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {status_char}"),
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(elapsed) = prompt.elapsed_display() {
            parts.push(Span::styled(
                format!(" {elapsed}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
        parts
    } else {
        vec![]
    };

    // --- Session elapsed time ---
    let session_secs = app.session_start.elapsed().as_secs();
    let session_str = if session_secs < 60 {
        format!("{session_secs}s")
    } else if session_secs < 3600 {
        format!("{}m{}s", session_secs / 60, session_secs % 60)
    } else {
        format!("{}h{}m", session_secs / 3600, (session_secs % 3600) / 60)
    };

    // --- Build spans ---
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            format!(" {mode_str} "),
            Style::default()
                .fg(Color::Black)
                .bg(mode_color)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if app.visual_select_active {
        spans.push(Span::styled(
            " VISUAL ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::LightBlue)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Disconnected indicator
    if !app.connected {
        spans.push(Span::styled(
            " [DISCONNECTED] ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }

    spans.extend([
        sep.clone(),
        Span::styled(bar_filled, Style::default().fg(Color::Cyan)),
        Span::styled(bar_empty, Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(" {}/{}", app.active_workers, app.max_workers),
            Style::default().fg(Color::Gray),
        ),
        sep.clone(),
        Span::styled(
            format!("Q:{pending}"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("D:{done}"),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("T:{total}"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    // Selection count indicator
    if app.selection_count() > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{} sel", app.selection_count()),
            Style::default()
                .fg(Color::LightBlue)
                .add_modifier(Modifier::BOLD),
        ));
    }

    spans.extend(selected_info);

    spans.push(sep.clone());
    spans.push(Span::styled(
        session_str,
        Style::default().fg(Color::DarkGray),
    ));

    spans.push(sep);
    spans.push(Span::styled(
        format!("[{}]", app.default_mode.label()),
        Style::default()
            .fg(match app.default_mode {
                PromptMode::Interactive => Color::Magenta,
                PromptMode::OneShot => Color::Yellow,
            })
            .add_modifier(Modifier::BOLD),
    ));

    let paragraph = Paragraph::new(Line::from(spans))
        .style(Style::default().bg(Color::Rgb(30, 30, 40)))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(mode_color)),
        );
    f.render_widget(paragraph, area);
}

fn render_main_area(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let list_pct = if app.list_collapsed {
        0
    } else {
        app.list_ratio
    };
    let output_pct = 100 - list_pct;

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(list_pct),
            Constraint::Percentage(output_pct),
        ])
        .split(area);

    if !app.list_collapsed {
        render_prompt_list(f, app, chunks[0]);
    }
    render_output_viewer(f, app, chunks[1]);
}

fn truncate_prompt(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        text.to_string()
    } else if max_chars <= 3 {
        text.chars().take(max_chars).collect()
    } else {
        let truncated: String = text.chars().take(max_chars - 3).collect();
        format!("{truncated}...")
    }
}

/// Deterministic color for a tag name (hashed to a palette of distinct colors).
fn tag_color(tag: &str) -> Color {
    const PALETTE: &[Color] = &[
        Color::LightBlue,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightMagenta,
        Color::LightCyan,
        Color::LightRed,
        Color::Cyan,
        Color::Yellow,
    ];
    let hash: usize = tag.bytes().fold(0usize, |acc, b| {
        acc.wrapping_mul(31).wrapping_add(b as usize)
    });
    PALETTE[hash % PALETTE.len()]
}

fn render_prompt_list(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    // In Normal mode with a selected prompt, reserve space for preview pane
    let show_preview = app.mode == AppMode::Normal && app.list_state.selected().is_some();
    let preview_height: u16 = if show_preview { 5 } else { 0 }; // 3 content lines + 2 borders

    let (list_area, preview_area) = if show_preview && area.height > preview_height + 4 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(4), Constraint::Length(preview_height)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };

    app.list_height = list_area.height;
    let tick = app.tick;
    // Check for recently moved prompt (flash highlight for ~300ms)
    let moved_id = app.recently_moved.and_then(|(id, t)| {
        if t.elapsed().as_millis() < 300 {
            Some(id)
        } else {
            None
        }
    });
    let visible_indices = app.visible_prompt_indices().to_vec();

    // Available width for content: list_area minus borders (2) minus highlight symbol "▶ " (2)
    let content_width = (list_area.width as usize).saturating_sub(4);

    let items: Vec<ListItem> = visible_indices
        .iter()
        .map(|&idx| {
            let prompt = &app.prompts[idx];
            let status = prompt.status_enum();
            let elapsed = prompt
                .elapsed_display()
                .map(|d| format!(" ({d})"))
                .unwrap_or_default();

            let is_unseen_done = !prompt.seen
                && (status == PromptStatus::Completed || status == PromptStatus::Failed);

            let status_style = match status {
                PromptStatus::Pending => Style::default().fg(Color::Yellow),
                PromptStatus::Blocked => Style::default().fg(Color::DarkGray),
                PromptStatus::Running => Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                PromptStatus::Idle => Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
                PromptStatus::Completed => Style::default().fg(Color::Green),
                PromptStatus::Failed => Style::default().fg(Color::Red),
            };

            // Calculate display width of all non-text spans to give remaining space to prompt text
            // Status emoji (2 display cols) + space (1) = 3
            let id_str = format!("#{} ", prompt.id);
            let is_selected = app.is_selected(prompt.id);
            // "● " = 2 display cols when selected
            let mut overhead = 3 + id_str.len() + elapsed.len();
            if is_selected {
                overhead += 2;
            }

            if prompt.worktree {
                overhead += 5; // " [WT]"
            }

            // Tag badges: " [tag]" per tag
            for tag in &prompt.tags {
                overhead += tag.len() + 3; // " [" + tag + "]"
            }

            if let Some(ref dir) = prompt.cwd {
                // Matches the cwd_hint formatting below
                overhead += if dir.len() > 20 { 22 } else { dir.len() + 3 };
            }

            if status == PromptStatus::Idle {
                overhead += 7; // " " + " IDLE "
            } else if is_unseen_done {
                overhead += if status == PromptStatus::Completed {
                    8
                } else {
                    9
                };
            }

            let max_text_chars = content_width.saturating_sub(overhead).max(8);
            let truncated = truncate_prompt(&prompt.text, max_text_chars);

            let cwd_hint = prompt.cwd.as_ref().map(|dir| {
                let display = if dir.len() > 20 {
                    format!(" [..{}]", &dir[dir.len() - 18..])
                } else {
                    format!(" [{dir}]")
                };
                Span::styled(display, Style::default().fg(Color::Magenta))
            });

            let status_tag = if status == PromptStatus::Idle {
                let bright = (tick / 5).is_multiple_of(2);
                let style = if bright {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Magenta)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD)
                };
                Some(Span::styled(" IDLE ", style))
            } else if is_unseen_done {
                let tag = if status == PromptStatus::Completed {
                    " READY "
                } else {
                    " FAILED "
                };
                let tag_color = if status == PromptStatus::Completed {
                    Color::Green
                } else {
                    Color::Red
                };
                // Pulse between bright and dim every ~500ms (5 ticks at 100ms)
                let bright = (tick / 5).is_multiple_of(2);
                let style = if bright {
                    Style::default()
                        .fg(Color::Black)
                        .bg(tag_color)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(tag_color).add_modifier(Modifier::BOLD)
                };
                Some(Span::styled(tag, style))
            } else {
                None
            };

            let mut spans = Vec::new();
            if is_selected {
                spans.push(Span::styled("● ", Style::default().fg(Color::LightBlue)));
            }
            spans.extend([
                Span::styled(format!("{} ", prompt.status_symbol()), status_style),
                Span::styled(id_str, Style::default().fg(Color::DarkGray)),
                Span::raw(truncated),
                Span::styled(elapsed, Style::default().fg(Color::DarkGray)),
            ]);
            if prompt.worktree {
                spans.push(Span::styled(
                    " [WT]",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            for tag in &prompt.tags {
                spans.push(Span::styled(
                    format!(" [{tag}]"),
                    Style::default()
                        .fg(tag_color(tag))
                        .add_modifier(Modifier::BOLD),
                ));
            }
            if let Some(cwd_span) = cwd_hint {
                spans.push(cwd_span);
            }
            if let Some(tag) = status_tag {
                spans.push(Span::raw(" "));
                spans.push(tag);
            }

            let line = Line::from(spans);

            // Give unseen/idle/selected items a subtle background highlight
            let item = ListItem::new(line);
            if is_selected {
                return item.style(Style::default().bg(Color::Rgb(35, 40, 55)));
            }
            if moved_id == Some(prompt.id) {
                // Flash highlight for recently reordered prompt
                item.style(
                    Style::default()
                        .bg(Color::Rgb(60, 60, 30))
                        .add_modifier(Modifier::BOLD),
                )
            } else if status == PromptStatus::Idle {
                let bg = if (tick / 5).is_multiple_of(2) {
                    Color::Rgb(45, 30, 50)
                } else {
                    Color::Rgb(35, 25, 40)
                };
                item.style(Style::default().bg(bg))
            } else if is_unseen_done {
                let bg = if (tick / 5).is_multiple_of(2) {
                    Color::Rgb(40, 50, 30)
                } else {
                    Color::Rgb(30, 35, 25)
                };
                item.style(Style::default().bg(bg))
            } else {
                item
            }
        })
        .collect();

    // Build title with optional filter indicator
    let title = if let Some(ref filter) = app.filter_text {
        format!(" Prompts [filter: {filter}] ")
    } else {
        " Prompts ".to_string()
    };

    // Map the real selection index to the position in the filtered list
    let mut filtered_list_state = ListState::default();
    if let Some(selected) = app.list_state.selected() {
        let filtered_pos = visible_indices.iter().position(|&i| i == selected);
        filtered_list_state.select(filtered_pos);
    }

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(80, 80, 100)))
                .title(Span::styled(
                    title,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
        )
        .highlight_style(if moved_id.is_some() {
            Style::default()
                .bg(Color::Rgb(80, 80, 20))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .bg(Color::Rgb(40, 40, 60))
                .add_modifier(Modifier::BOLD)
        })
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, list_area, &mut filtered_list_state);

    // Render prompt preview pane
    if let Some(preview_rect) = preview_area {
        if let Some(selected) = app.list_state.selected() {
            let prompt = &app.prompts[selected];
            let session_line = prompt
                .session_id
                .as_ref()
                .map(|sid| format!("[session: {sid}]\n"))
                .unwrap_or_default();
            let preview_text = format!("{session_line}{}", prompt.text);
            let preview = Paragraph::new(preview_text)
                .style(Style::default().fg(Color::White))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Rgb(60, 60, 80)))
                        .title(Span::styled(
                            " Preview ",
                            Style::default().fg(Color::DarkGray),
                        )),
                )
                .wrap(Wrap { trim: false });
            f.render_widget(preview, preview_rect);
        }
    }
}

fn render_output_viewer(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    // Check if we should render the PTY grid
    if let Some(prompt) = app.selected_prompt() {
        let id = prompt.id;
        if app.pty_renderers.contains_key(&id) {
            let cwd_str = prompt.cwd.as_deref().unwrap_or(".").to_string();
            let is_pty_interact = app.mode == AppMode::PtyInteract;
            render_pty_output_viewer(f, app, area, id, &cwd_str, is_pty_interact);
            return;
        }
    }
    render_text_output_viewer(f, app, area);
}

fn render_pty_output_viewer(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    id: usize,
    cwd_str: &str,
    is_pty_interact: bool,
) {
    // Show [WT] in PTY title if this prompt has a worktree
    let wt_tag = if app
        .selected_prompt()
        .is_some_and(|p| p.worktree_path.is_some())
    {
        " [WT]"
    } else {
        ""
    };
    let title = format!(" PTY: #{id} [{cwd_str}]{wt_tag} ");
    let live_indicator = if is_pty_interact {
        Span::styled(
            " [LIVE] ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("")
    };

    // Status message indicator
    let status_indicator = if let Some((ref msg, _)) = app.status_message {
        Span::styled(
            format!(" {msg} "),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("")
    };

    // PTY scrollback indicator
    let scroll_indicator = app
        .pty_renderers
        .get(&id)
        .map(|r| r.display_offset())
        .filter(|&offset| offset > 0)
        .map(|offset| {
            Span::styled(
                format!(" [+{offset} lines] "),
                Style::default().fg(Color::Yellow),
            )
        })
        .unwrap_or_else(|| Span::raw(""));

    let border_color = if is_pty_interact {
        Color::Green
    } else {
        Color::Cyan
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(vec![
            Span::styled(
                title,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            live_indicator,
            scroll_indicator,
            status_indicator,
        ]);

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Update output panel size and position for PTY resize tracking + mouse mapping
    app.output_panel_size = Some((inner.width, inner.height));
    app.output_panel_rect = Some((inner.x, inner.y, inner.width, inner.height));

    // Render PTY grid content
    if let Some(renderer) = app.pty_renderers.get(&id) {
        render_pty_grid(f, renderer, inner);
    }
}

fn render_pty_grid(f: &mut Frame, renderer: &PtyRenderer, area: Rect) {
    let term = renderer.term();
    let grid = term.grid();
    let screen_lines = grid.screen_lines();
    let cols = grid.columns();

    let render_rows = (area.height as usize).min(screen_lines);
    let render_cols = (area.width as usize).min(cols);

    for row in 0..render_rows {
        let line = ALine(row as i32);
        let mut spans: Vec<Span> = Vec::new();
        let mut current_text = String::new();
        let mut current_style = Style::default();

        for col in 0..render_cols {
            let cell = &grid[line][Column(col)];

            // Skip wide char spacers
            if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                continue;
            }

            let style = cell_style(cell.fg, cell.bg, cell.flags);

            if style == current_style {
                current_text.push(cell.c);
            } else {
                if !current_text.is_empty() {
                    spans.push(Span::styled(current_text.clone(), current_style));
                    current_text.clear();
                }
                current_style = style;
                current_text.push(cell.c);
            }
        }
        if !current_text.is_empty() {
            spans.push(Span::styled(current_text, current_style));
        }

        let line_widget = Line::from(spans);
        let row_area = Rect {
            x: area.x,
            y: area.y + row as u16,
            width: area.width,
            height: 1,
        };
        f.render_widget(Paragraph::new(line_widget), row_area);
    }
}

fn cell_style(fg: AColor, bg: AColor, flags: CellFlags) -> Style {
    let mut style = Style::default();
    style = style.fg(convert_color(fg, false));
    style = style.bg(convert_color(bg, true));
    style = style.add_modifier(convert_flags(flags));
    style
}

fn convert_color(color: AColor, _is_bg: bool) -> Color {
    match color {
        AColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AColor::Indexed(n) => Color::Indexed(n),
        AColor::Named(name) => match name {
            NamedColor::Black | NamedColor::DimBlack => Color::Black,
            NamedColor::Red | NamedColor::DimRed => Color::Red,
            NamedColor::Green | NamedColor::DimGreen => Color::Green,
            NamedColor::Yellow | NamedColor::DimYellow => Color::Yellow,
            NamedColor::Blue | NamedColor::DimBlue => Color::Blue,
            NamedColor::Magenta | NamedColor::DimMagenta => Color::Magenta,
            NamedColor::Cyan | NamedColor::DimCyan => Color::Cyan,
            NamedColor::White | NamedColor::DimWhite => Color::White,
            NamedColor::BrightBlack => Color::DarkGray,
            NamedColor::BrightRed => Color::LightRed,
            NamedColor::BrightGreen => Color::LightGreen,
            NamedColor::BrightYellow => Color::LightYellow,
            NamedColor::BrightBlue => Color::LightBlue,
            NamedColor::BrightMagenta => Color::LightMagenta,
            NamedColor::BrightCyan => Color::LightCyan,
            NamedColor::BrightWhite => Color::White,
            NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::DimForeground => {
                Color::Reset
            }
            NamedColor::Background => Color::Reset,
            NamedColor::Cursor => Color::Reset,
        },
    }
}

fn convert_flags(flags: CellFlags) -> Modifier {
    let mut modifier = Modifier::empty();
    if flags.contains(CellFlags::BOLD) {
        modifier |= Modifier::BOLD;
    }
    if flags.contains(CellFlags::ITALIC) {
        modifier |= Modifier::ITALIC;
    }
    if flags.contains(CellFlags::UNDERLINE) {
        modifier |= Modifier::UNDERLINED;
    }
    if flags.contains(CellFlags::DIM) {
        modifier |= Modifier::DIM;
    }
    if flags.contains(CellFlags::INVERSE) {
        modifier |= Modifier::REVERSED;
    }
    if flags.contains(CellFlags::STRIKEOUT) {
        modifier |= Modifier::CROSSED_OUT;
    }
    if flags.contains(CellFlags::HIDDEN) {
        modifier |= Modifier::HIDDEN;
    }
    modifier
}

fn render_text_output_viewer(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let (title, content) = match app.selected_prompt() {
        Some(prompt) => {
            let status = prompt.status_enum();
            let mode = prompt.mode_enum();
            let cwd_str = prompt.cwd.as_deref().unwrap_or(".");
            let wt_tag = if prompt.worktree_path.is_some() {
                " [WT]"
            } else {
                ""
            };
            let title = format!(" Output: #{} [{}]{wt_tag} ", prompt.id, cwd_str);
            let content = match status {
                PromptStatus::Pending => "(pending)".to_string(),
                PromptStatus::Blocked => {
                    if prompt.blocked_by.is_empty() {
                        "(blocked)".to_string()
                    } else {
                        format!("(blocked — waiting on {} dependency(ies))", prompt.blocked_by.len())
                    }
                }
                PromptStatus::Running => {
                    let elapsed = prompt.elapsed_display().unwrap_or_else(|| "0.0s".into());
                    match &prompt.output {
                        Some(output) => {
                            format!("Running... ({elapsed})\n\n{output}")
                        }
                        None => format!("Running... ({elapsed})"),
                    }
                }
                PromptStatus::Idle => {
                    let elapsed = prompt.elapsed_display().unwrap_or_else(|| "0.0s".into());
                    let hint = if mode == PromptMode::Interactive {
                        let key = app.keymap.view_key_hint(ViewAction::Interact);
                        format!(" — press '{key}' to interact")
                    } else {
                        String::new()
                    };
                    match &prompt.output {
                        Some(output) => {
                            format!("{output}\n\n— Idle ({elapsed}){hint}")
                        }
                        None => format!("Idle ({elapsed}){hint}"),
                    }
                }
                PromptStatus::Completed => prompt
                    .output
                    .clone()
                    .unwrap_or_else(|| "(no output)".to_string()),
                PromptStatus::Failed => {
                    let mut text = String::from("FAILED");
                    if let Some(err) = &prompt.error {
                        text.push_str(&format!(":\n{err}"));
                    }
                    if let Some(output) = &prompt.output {
                        if !output.is_empty() {
                            text.push_str(&format!("\n\nOutput:\n{output}"));
                        }
                    }
                    text
                }
            };
            (title, content)
        }
        None => (
            " Output ".to_string(),
            "Select a prompt to view output".to_string(),
        ),
    };

    // Auto-scroll: compute scroll offset to show the bottom of content
    if app.auto_scroll && matches!(app.mode, AppMode::ViewOutput | AppMode::Interact) {
        if let Some(prompt) = app.selected_prompt() {
            if prompt.status_enum() == PromptStatus::Running {
                // Estimate total lines (rough: count newlines + wrapping)
                let inner_height = area.height.saturating_sub(2); // borders
                let line_count = content.lines().count() as u16;
                if line_count > inner_height {
                    app.scroll_offset = line_count.saturating_sub(inner_height);
                }
            }
        }
    }

    let auto_scroll_indicator = if app.auto_scroll {
        Span::styled(" [auto-scroll] ", Style::default().fg(Color::Green))
    } else {
        Span::raw("")
    };

    // Status message indicator (transient, shown for 3s)
    let status_indicator = if let Some((ref msg, _)) = app.status_message {
        Span::styled(
            format!(" {msg} "),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("")
    };

    let output_border_color = if let Some(prompt) = app.selected_prompt() {
        match prompt.status_enum() {
            PromptStatus::Running => Color::Cyan,
            PromptStatus::Idle => Color::Magenta,
            PromptStatus::Completed => Color::Green,
            PromptStatus::Failed => Color::Red,
            PromptStatus::Pending => Color::Yellow,
            PromptStatus::Blocked => Color::DarkGray,
        }
    } else {
        Color::Rgb(80, 80, 100)
    };

    let paragraph = Paragraph::new(content)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(output_border_color))
                .title(vec![
                    Span::styled(
                        title,
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    auto_scroll_indicator,
                    status_indicator,
                ]),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.scroll_offset, 0));
    f.render_widget(paragraph, area);
}

fn render_input_bar(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let (title, content, style, border_color): (String, String, Style, Color) = match app.mode {
        AppMode::Insert => {
            let wt_tag = if app.worktree_pending { " [WT]" } else { "" };
            let line_tag = if app.input.is_multiline() {
                let (row, _) = app.input.cursor();
                format!(" [L{}/{}]", row + 1, app.input.line_count())
            } else {
                String::new()
            };
            (
                format!(" Input (Enter to submit, Esc to cancel){wt_tag}{line_tag} "),
                app.input.to_string(),
                Style::default().fg(Color::White),
                if app.worktree_pending {
                    Color::Cyan
                } else {
                    Color::Green
                },
            )
        }
        AppMode::Interact => (
            " Interact (Enter to send, Esc to cancel) ".to_string(),
            app.interact_input.clone(),
            Style::default().fg(Color::Cyan),
            Color::Magenta,
        ),
        AppMode::Filter => (
            " Filter (Enter to apply, Esc to cancel) ".to_string(),
            app.filter_input.clone(),
            Style::default().fg(Color::White),
            Color::Cyan,
        ),
        AppMode::PtyInteract => (
            " PTY Interactive (Esc to exit) ".to_string(),
            String::new(),
            Style::default().fg(Color::DarkGray),
            Color::Green,
        ),
        _ => {
            let key = app.keymap.normal_key_hint(NormalAction::Insert);
            (
                format!(" Input (press '{key}' to enter a prompt) "),
                String::new(),
                Style::default().fg(Color::DarkGray),
                Color::Rgb(80, 80, 100),
            )
        }
    };

    let paragraph = Paragraph::new(content).style(style).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(Span::styled(title, Style::default().fg(border_color))),
    );
    f.render_widget(paragraph, area);

    match app.mode {
        AppMode::Insert => {
            let (row, col) = app.input.cursor();
            let x = area.x + col as u16 + 1; // +1 for border
            let y = area.y + row as u16 + 1; // +1 for border
            f.set_cursor_position((x, y));
        }
        AppMode::Interact => {
            let x = area.x + app.interact_input.len() as u16 + 1;
            let y = area.y + 1;
            f.set_cursor_position((x, y));
        }
        AppMode::Filter => {
            let x = area.x + app.filter_input.len() as u16 + 1;
            let y = area.y + 1;
            f.set_cursor_position((x, y));
        }
        _ => {}
    }
}

fn render_suggestions(f: &mut Frame, app: &App, input_area: Rect) {
    if app.mode != AppMode::Insert || app.suggestions.is_empty() {
        return;
    }

    let visible = app.suggestions.len().min(5) as u16;
    let height = visible + 2; // +2 for borders

    // Position popup above the input bar
    let popup_area = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(height),
        width: input_area.width.min(50),
        height,
    };

    let items: Vec<ListItem> = app
        .suggestions
        .iter()
        .enumerate()
        .take(5)
        .map(|(i, path)| {
            let style = if i == app.suggestion_index {
                Style::default()
                    .fg(Color::White)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Magenta)
            };
            ListItem::new(Span::styled(path.as_str(), style))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Magenta))
            .title(Span::styled(
                " Directories (Tab to select, Up/Down to navigate) ",
                Style::default().fg(Color::Magenta),
            )),
    );

    f.render_widget(Clear, popup_area);
    f.render_widget(list, popup_area);
}

fn render_template_suggestions(f: &mut Frame, app: &App, input_area: Rect) {
    if app.mode != AppMode::Insert || app.template_suggestions.is_empty() {
        return;
    }
    // Don't show if directory suggestions are visible
    if !app.suggestions.is_empty() {
        return;
    }

    let visible = app.template_suggestions.len().min(5) as u16;
    let height = visible + 2;

    let popup_area = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(height),
        width: input_area.width.min(60),
        height,
    };

    let items: Vec<ListItem> = app
        .template_suggestions
        .iter()
        .enumerate()
        .take(5)
        .map(|(i, name)| {
            let preview = app
                .templates
                .get(name)
                .map(|t| {
                    if t.len() > 40 {
                        format!("{}...", &t[..37])
                    } else {
                        t.clone()
                    }
                })
                .unwrap_or_default();

            let style = if i == app.template_suggestion_index {
                Style::default()
                    .fg(Color::White)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan)
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(":{name} "), style),
                Span::styled(preview, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                " Templates (Tab to select) ",
                Style::default().fg(Color::Cyan),
            )),
    );

    f.render_widget(Clear, popup_area);
    f.render_widget(list, popup_area);
}

fn render_quit_confirmation(f: &mut Frame, area: Rect) {
    let width = 44;
    let height = 5;
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup_area = Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    };

    let text = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  Workers still active. Quit? "),
            Span::styled(
                "y",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("/"),
            Span::styled(
                "n",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ]),
    ];

    let paragraph = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(Span::styled(
                    " Confirm Quit ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
        )
        .style(Style::default().bg(Color::Rgb(40, 30, 30)));

    f.render_widget(Clear, popup_area);
    f.render_widget(paragraph, popup_area);
}

fn render_batch_delete_confirmation(f: &mut Frame, app: &App, area: Rect) {
    let count = app.selection_count();
    let msg = format!(
        "  Delete {count} prompt{}? ",
        if count == 1 { "" } else { "s" }
    );
    let width = (msg.len() as u16 + 8).max(36);
    let height = 5;
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup_area = Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    };

    let text = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw(msg),
            Span::styled(
                "y",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("/"),
            Span::styled(
                "n",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ]),
    ];

    let paragraph = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red))
                .title(Span::styled(
                    " Confirm Delete ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )),
        )
        .style(Style::default().bg(Color::Rgb(40, 25, 25)));

    f.render_widget(Clear, popup_area);
    f.render_widget(paragraph, popup_area);
}

fn render_quick_prompts_popup(f: &mut Frame, app: &App, main_area: Rect) {
    let qp = app.keymap.quick_prompt_help();

    // Compute the output panel area (matches render_main_area split)
    let list_pct = if app.list_collapsed {
        0
    } else {
        app.list_ratio
    };
    let output_pct = 100 - list_pct;
    let output_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(list_pct),
            Constraint::Percentage(output_pct),
        ])
        .split(main_area)[1];

    let lines: Vec<Line> = if qp.is_empty() {
        vec![Line::from(Span::styled(
            "  No quick prompts configured.",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        qp.iter()
            .map(|(key, msg)| {
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("{key:>3}"),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(msg.as_str(), Style::default().fg(Color::Gray)),
                ])
            })
            .collect()
    };

    let content_height = lines.len() as u16 + 2; // +2 for borders
    let max_width: u16 = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.len() as u16).sum::<u16>())
        .max()
        .unwrap_or(30)
        + 4; // padding
    let width = max_width.min(60).min(output_area.width.saturating_sub(4));
    let height = content_height.min(output_area.height.saturating_sub(2));

    // Center in the output panel
    let x = output_area.x + (output_area.width.saturating_sub(width)) / 2;
    let y = output_area.y + (output_area.height.saturating_sub(height)) / 2;

    let popup_area = Rect {
        x,
        y,
        width,
        height,
    };

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(Span::styled(
                    " Quick Prompts ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ))
                .title_bottom(Line::from(Span::styled(
                    " Esc to close ",
                    Style::default().fg(Color::DarkGray),
                ))),
        )
        .style(Style::default().bg(Color::Rgb(30, 30, 40)));

    f.render_widget(Clear, popup_area);
    f.render_widget(paragraph, popup_area);
}

fn render_help_overlay(f: &mut Frame, app: &App, area: Rect) {
    let title_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let key_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(Color::Gray);
    let section_style = Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);

    let mut lines: Vec<Line> = Vec::new();

    // Helper to add a section
    let mut add_section = |name: &str, bindings: &[(String, &str)], extras: &[(&str, &str)]| {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(format!("  {name}"), section_style)));
        for (key, desc) in bindings {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(format!("{key:>12}"), key_style),
                Span::raw("  "),
                Span::styled(desc.to_string(), desc_style),
            ]));
        }
        for (key, desc) in extras {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(format!("{key:>12}"), key_style),
                Span::raw("  "),
                Span::styled(desc.to_string(), desc_style),
            ]));
        }
    };

    // NORMAL
    let normal = app.keymap.normal_help();
    add_section(
        "NORMAL",
        &normal,
        &[
            ("Ctrl+D", "half page down"),
            ("Ctrl+U", "half page up"),
            ("gg", "go to top"),
        ],
    );

    // SELECTION
    add_section(
        "SELECTION (normal mode)",
        &[],
        &[
            (
                &app.keymap.normal_key_hint(NormalAction::ToggleSelect),
                "toggle select",
            ),
            (
                &app.keymap.normal_key_hint(NormalAction::VisualSelect),
                "visual select",
            ),
            (
                &app.keymap.normal_key_hint(NormalAction::SelectAllVisible),
                "select all visible",
            ),
            (
                &app.keymap.normal_key_hint(NormalAction::DeleteSelected),
                "delete selected",
            ),
            (
                &app.keymap.normal_key_hint(NormalAction::KillSelected),
                "kill selected",
            ),
            ("Esc", "clear selection"),
        ],
    );

    // INSERT
    let insert = app.keymap.insert_help();
    add_section(
        "INSERT",
        &insert,
        &[
            ("Shift+Enter", "insert newline"),
            ("Ctrl+E", "open $EDITOR"),
            ("Ctrl+W", "toggle worktree"),
            ("Left/Right", "move cursor"),
            ("Home/End", "line start/end"),
            ("Up/Down", "navigate lines / history"),
            (":name+Tab", "expand template"),
        ],
    );

    // VIEW
    let view = app.keymap.view_help();
    add_section("VIEW", &view, &[("Ctrl+P", "quick prompts")]);

    // INTERACT
    let interact = app.keymap.interact_help();
    add_section("INTERACT", &interact, &[]);

    // FILTER
    let filter = app.keymap.filter_help();
    add_section("FILTER", &filter, &[]);

    // PTY INTERACT
    add_section(
        "PTY INTERACT",
        &[],
        &[("Esc", "exit PTY mode"), ("*", "all keys forwarded to PTY")],
    );

    // Quick prompts section (if any configured)
    let qp = app.keymap.quick_prompt_help();
    if !qp.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  QUICK PROMPTS (view mode)",
            section_style,
        )));
        for (key, msg) in &qp {
            let display_msg = if msg.len() > 40 {
                format!("{}...", &msg[..37])
            } else {
                msg.clone()
            };
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(format!("{key:>12}"), key_style),
                Span::raw("  "),
                Span::styled(display_msg, desc_style),
            ]));
        }
    }

    lines.push(Line::from(""));

    // Apply scroll
    let total_lines = lines.len() as u16;
    let inner_height = area.height.saturating_sub(4); // borders + footer
    let max_scroll = total_lines.saturating_sub(inner_height);
    let scroll = app.help_scroll.min(max_scroll);

    let paragraph = Paragraph::new(lines)
        .scroll((scroll, 0))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(Span::styled(" Keybindings ", title_style))
                .title_bottom(Line::from(vec![
                    Span::styled(" ?", key_style),
                    Span::styled("/", desc_style),
                    Span::styled("Esc", key_style),
                    Span::styled("/", desc_style),
                    Span::styled("q", key_style),
                    Span::styled(" to close", desc_style),
                    Span::raw("  "),
                    Span::styled("j", key_style),
                    Span::styled("/", desc_style),
                    Span::styled("k", key_style),
                    Span::styled(" to scroll ", desc_style),
                ])),
        )
        .style(Style::default().bg(Color::Rgb(20, 20, 30)));

    f.render_widget(Clear, area);
    f.render_widget(paragraph, area);
}

fn render_help_bar(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    // On Drafts / Workflows tabs the keymap is much smaller — the
    // existing per-mode hints don't apply. Surface the navigation +
    // refresh + tab-switch keys instead.
    if app.mode == AppMode::Normal && app.root_view != RootView::Prompts {
        let label = match app.root_view {
            RootView::Drafts => "drafts",
            RootView::Workflows => {
                if app.workflow_detail.is_some() {
                    "detail"
                } else {
                    "workflows"
                }
            }
            RootView::Prompts => "prompts",
        };
        // Tab-specific action keys: Drafts get Q+E+R, Workflows get
        // X+T+R + Enter to zoom. Both share the navigation/quit/refresh
        // row. The detail overlay swaps in its own hints.
        let bindings: Vec<(String, &str)> = match (app.root_view, app.workflow_detail.is_some()) {
            (RootView::Workflows, true) => vec![
                ("j/k".to_string(), "scroll"),
                ("X".to_string(), "cancel"),
                ("T".to_string(), "retry §"),
                ("R".to_string(), "read"),
                ("r".to_string(), "refresh"),
                ("Esc".to_string(), "back"),
                ("q".to_string(), "quit"),
            ],
            (RootView::Drafts, _) => vec![
                ("j/k".to_string(), "navigate"),
                ("Q".to_string(), "queue"),
                ("E".to_string(), "explore"),
                ("R".to_string(), "read"),
                ("r".to_string(), "refresh"),
                ("Esc".to_string(), "→ Prompts"),
                ("q".to_string(), "quit"),
            ],
            (RootView::Workflows, false) => vec![
                ("j/k".to_string(), "navigate"),
                ("Enter".to_string(), "detail"),
                ("X".to_string(), "cancel"),
                ("T".to_string(), "retry §"),
                ("R".to_string(), "read"),
                ("r".to_string(), "refresh"),
                ("Esc".to_string(), "→ Prompts"),
                ("q".to_string(), "quit"),
            ],
            (RootView::Prompts, _) => vec![
                ("j/k".to_string(), "navigate"),
                ("r".to_string(), "refresh"),
                ("1/2/3".to_string(), "switch tab"),
                ("Esc".to_string(), "→ Prompts"),
                ("?".to_string(), "help"),
                ("q".to_string(), "quit"),
            ],
        };
        let mut spans: Vec<Span> = vec![
            Span::raw(" "),
            Span::styled(
                format!("{label:<10}"),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
        ];
        for (i, (key, desc)) in bindings.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(
                    "  ",
                    Style::default().fg(Color::Rgb(60, 60, 60)),
                ));
            }
            spans.push(Span::styled(
                key.as_str(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                *desc,
                Style::default().fg(Color::Gray),
            ));
        }
        let para = Paragraph::new(Line::from(spans))
            .style(Style::default().bg(Color::Rgb(20, 20, 30)));
        f.render_widget(para, area);
        return;
    }

    let bindings: Vec<(String, &str)> = match app.mode {
        AppMode::Normal if app.visual_select_active => {
            vec![
                (
                    app.keymap.normal_key_hint(NormalAction::SelectNext),
                    "extend",
                ),
                (
                    app.keymap.normal_key_hint(NormalAction::ToggleSelect),
                    "toggle",
                ),
                ("Esc".to_string(), "clear sel"),
            ]
        }
        AppMode::Normal if app.selection_count() > 0 => {
            vec![
                (app.keymap.normal_key_hint(NormalAction::Retry), "retry"),
                (
                    app.keymap.normal_key_hint(NormalAction::KillSelected),
                    "kill",
                ),
                (
                    app.keymap.normal_key_hint(NormalAction::DeleteSelected),
                    "delete",
                ),
                (app.keymap.normal_key_hint(NormalAction::ToggleMode), "mode"),
                (
                    app.keymap.normal_key_hint(NormalAction::ToggleSelect),
                    "toggle",
                ),
                (
                    app.keymap.normal_key_hint(NormalAction::SelectAllVisible),
                    "sel all",
                ),
                ("Esc".to_string(), "clear sel"),
            ]
        }
        AppMode::Normal => {
            let mut help = app.keymap.normal_help();
            if let Some(p) = app.selected_prompt() {
                let status = p.status_enum();
                let mode = p.mode_enum();
                let is_pending = status == PromptStatus::Pending;
                let is_running = matches!(status, PromptStatus::Running | PromptStatus::Idle);
                let is_finished = matches!(status, PromptStatus::Completed | PromptStatus::Failed);
                let is_interactive = mode == PromptMode::Interactive;
                help.retain(|(_, desc)| match *desc {
                    "move up" | "move down" => is_pending,
                    "interact" => is_interactive && is_running,
                    "retry" | "resume" => is_finished,
                    _ => true,
                });
            }
            help
        }
        AppMode::Insert => {
            let mut help = app.keymap.insert_help();
            help.push(("S-Ret".to_string(), "newline"));
            help.push(("C-e".to_string(), "editor"));
            help.push(("C-w".to_string(), "worktree"));
            help
        }
        AppMode::ViewOutput => app.keymap.view_help(),
        AppMode::Interact => app.keymap.interact_help(),
        AppMode::PtyInteract => vec![("Esc".to_string(), "exit PTY mode")],
        AppMode::Filter => app.keymap.filter_help(),
    };

    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    for (i, (key, desc)) in bindings.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                "  ",
                Style::default().fg(Color::Rgb(60, 60, 60)),
            ));
        }
        spans.push(Span::styled(
            key.as_str(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(":{desc}"),
            Style::default().fg(Color::Gray),
        ));
    }

    // In view mode, append quick prompt hints and Ctrl+P
    if app.mode == AppMode::ViewOutput {
        let qp = app.keymap.quick_prompt_help();
        if !qp.is_empty() {
            spans.push(Span::styled(
                " \u{2502} ",
                Style::default().fg(Color::Rgb(60, 60, 60)),
            ));
            let show_count = qp.len().min(3);
            for (i, (key, msg)) in qp.iter().take(show_count).enumerate() {
                if i > 0 {
                    spans.push(Span::styled(
                        "  ",
                        Style::default().fg(Color::Rgb(60, 60, 60)),
                    ));
                }
                spans.push(Span::styled(
                    key.clone(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
                let display_msg = if msg.len() > 15 {
                    format!(":{}…", &msg[..14])
                } else {
                    format!(":{msg}")
                };
                spans.push(Span::styled(display_msg, Style::default().fg(Color::Gray)));
            }
            if qp.len() > 3 {
                spans.push(Span::styled(
                    format!(" +{}", qp.len() - 3),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }
        spans.push(Span::styled(
            " \u{2502} ",
            Style::default().fg(Color::Rgb(60, 60, 60)),
        ));
        spans.push(Span::styled(
            "C-p",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            ":all prompts",
            Style::default().fg(Color::Gray),
        ));
    }

    // Show transient status message at the right end of the help bar
    if let Some((ref msg, _)) = app.status_message {
        spans.push(Span::styled(
            " │ ",
            Style::default().fg(Color::Rgb(60, 60, 60)),
        ));
        spans.push(Span::styled(
            msg.as_str(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let paragraph = Paragraph::new(Line::from(spans));
    f.render_widget(paragraph, area);
}
