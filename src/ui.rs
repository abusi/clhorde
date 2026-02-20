use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line as ALine};
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::vte::ansi::{Color as AColor, NamedColor};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, AppMode};
use crate::keymap::{NormalAction, ViewAction};
use crate::prompt::{PromptMode, PromptStatus};
use crate::pty_worker::SharedPtyState;

pub fn render(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),  // status bar (1 content + bottom border)
            Constraint::Min(5),    // main area
            Constraint::Length(3), // input bar
            Constraint::Length(1), // help bar
        ])
        .split(f.area());

    render_status_bar(f, app, chunks[0]);
    render_main_area(f, app, chunks[1]);
    render_input_bar(f, app, chunks[2]);
    render_help_bar(f, app, chunks[3]);
    render_suggestions(f, app, chunks[2]);
    render_template_suggestions(f, app, chunks[2]);

    if app.show_quick_prompts_popup
        && (app.mode == AppMode::ViewOutput || app.mode == AppMode::PtyInteract)
    {
        render_quick_prompts_popup(f, app, chunks[1]);
    }

    if app.confirm_quit {
        render_quit_confirmation(f, f.area());
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
        let (status_char, status_color) = match prompt.status {
            PromptStatus::Pending => ("·", Color::DarkGray),
            PromptStatus::Running => ("▶", Color::Cyan),
            PromptStatus::Idle => ("◆", Color::Magenta),
            PromptStatus::Completed => ("✓", Color::Green),
            PromptStatus::Failed => ("✗", Color::Red),
        };
        let mut parts = vec![
            sep.clone(),
            Span::styled(
                format!("#{}", prompt.id),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {status_char}"),
                Style::default().fg(status_color).add_modifier(Modifier::BOLD),
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
            Style::default().fg(Color::Black).bg(mode_color).add_modifier(Modifier::BOLD),
        ),
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
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("D:{done}"),
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("T:{total}"),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
    ];

    spans.extend(selected_info);

    spans.push(sep.clone());
    spans.push(Span::styled(
        session_str,
        Style::default().fg(Color::DarkGray),
    ));

    spans.push(sep);
    spans.push(Span::styled(
        format!("[{}]", app.default_mode.label()),
        Style::default().fg(match app.default_mode {
            PromptMode::Interactive => Color::Magenta,
            PromptMode::OneShot => Color::Yellow,
        }).add_modifier(Modifier::BOLD),
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
    let list_pct = if app.list_collapsed { 0 } else { app.list_ratio };
    let output_pct = 100 - list_pct;

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(list_pct), Constraint::Percentage(output_pct)])
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
    let visible_indices = app.visible_prompt_indices().to_vec();

    // Available width for content: list_area minus borders (2) minus highlight symbol "▶ " (2)
    let content_width = (list_area.width as usize).saturating_sub(4);

    let items: Vec<ListItem> = visible_indices
        .iter()
        .map(|&idx| {
            let prompt = &app.prompts[idx];
            let elapsed = prompt
                .elapsed_display()
                .map(|d| format!(" ({d})"))
                .unwrap_or_default();

            let is_unseen_done = !prompt.seen
                && (prompt.status == PromptStatus::Completed
                    || prompt.status == PromptStatus::Failed);

            let status_style = match prompt.status {
                PromptStatus::Pending => Style::default().fg(Color::Yellow),
                PromptStatus::Running => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                PromptStatus::Idle => Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                PromptStatus::Completed => Style::default().fg(Color::Green),
                PromptStatus::Failed => Style::default().fg(Color::Red),
            };

            // Calculate display width of all non-text spans to give remaining space to prompt text
            // Status emoji (2 display cols) + space (1) = 3
            let id_str = format!("#{} ", prompt.id);
            let mut overhead = 3 + id_str.len() + elapsed.len();

            if prompt.worktree {
                overhead += 5; // " [WT]"
            }

            if let Some(ref dir) = prompt.cwd {
                // Matches the cwd_hint formatting below
                overhead += if dir.len() > 20 { 22 } else { dir.len() + 3 };
            }

            if prompt.status == PromptStatus::Idle {
                overhead += 7; // " " + " IDLE "
            } else if is_unseen_done {
                overhead += if prompt.status == PromptStatus::Completed { 8 } else { 9 };
            }

            let max_text_chars = content_width.saturating_sub(overhead).max(8);
            let truncated = truncate_prompt(&prompt.text, max_text_chars);

            let cwd_hint = prompt.cwd.as_ref().map(|dir| {
                let display = if dir.len() > 20 {
                    format!(" [..{}]", &dir[dir.len()-18..])
                } else {
                    format!(" [{dir}]")
                };
                Span::styled(display, Style::default().fg(Color::Magenta))
            });

            let status_tag = if prompt.status == PromptStatus::Idle {
                let bright = (tick / 5) % 2 == 0;
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
                let tag = if prompt.status == PromptStatus::Completed {
                    " READY "
                } else {
                    " FAILED "
                };
                let tag_color = if prompt.status == PromptStatus::Completed {
                    Color::Green
                } else {
                    Color::Red
                };
                // Pulse between bright and dim every ~500ms (5 ticks at 100ms)
                let bright = (tick / 5) % 2 == 0;
                let style = if bright {
                    Style::default()
                        .fg(Color::Black)
                        .bg(tag_color)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(tag_color)
                        .add_modifier(Modifier::BOLD)
                };
                Some(Span::styled(tag, style))
            } else {
                None
            };

            let mut spans = vec![
                Span::styled(
                    format!("{} ", prompt.status.symbol()),
                    status_style,
                ),
                Span::styled(
                    id_str,
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(truncated),
                Span::styled(elapsed, Style::default().fg(Color::DarkGray)),
            ];
            if prompt.worktree {
                spans.push(Span::styled(" [WT]", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
            }
            if let Some(cwd_span) = cwd_hint {
                spans.push(cwd_span);
            }
            if let Some(tag) = status_tag {
                spans.push(Span::raw(" "));
                spans.push(tag);
            }

            let line = Line::from(spans);

            // Give unseen/idle items a subtle background highlight
            let item = ListItem::new(line);
            if prompt.status == PromptStatus::Idle {
                let bg = if (tick / 5) % 2 == 0 {
                    Color::Rgb(45, 30, 50)
                } else {
                    Color::Rgb(35, 25, 40)
                };
                item.style(Style::default().bg(bg))
            } else if is_unseen_done {
                let bg = if (tick / 5) % 2 == 0 {
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
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 40, 60))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, list_area, &mut filtered_list_state);

    // Render prompt preview pane
    if let Some(preview_rect) = preview_area {
        if let Some(selected) = app.list_state.selected() {
            let prompt_text = &app.prompts[selected].text;
            let preview = Paragraph::new(prompt_text.as_str())
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
        if prompt.pty_state.is_some() {
            let pty_state = prompt.pty_state.clone().unwrap();
            let id = prompt.id;
            let cwd_str = prompt.cwd.as_deref().unwrap_or(".").to_string();
            let is_pty_interact = app.mode == AppMode::PtyInteract;
            render_pty_output_viewer(f, app, &pty_state, area, id, &cwd_str, is_pty_interact);
            return;
        }
    }
    render_text_output_viewer(f, app, area);
}

fn render_pty_output_viewer(
    f: &mut Frame,
    app: &mut App,
    pty_state: &SharedPtyState,
    area: Rect,
    id: usize,
    cwd_str: &str,
    is_pty_interact: bool,
) {
    // Show [WT] in PTY title if this prompt has a worktree
    let wt_tag = if app.selected_prompt().is_some_and(|p| p.worktree) {
        " [WT]"
    } else {
        ""
    };
    let title = format!(" PTY: #{id} [{cwd_str}]{wt_tag} ");
    let live_indicator = if is_pty_interact {
        Span::styled(" [LIVE] ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
    } else {
        Span::raw("")
    };

    // Status message indicator
    let status_indicator = if let Some((ref msg, _)) = app.status_message {
        Span::styled(
            format!(" {msg} "),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("")
    };

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
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            live_indicator,
            status_indicator,
        ]);

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Update output panel size for PTY resize tracking
    app.output_panel_size = Some((inner.width, inner.height));

    // Render PTY grid content
    render_pty_grid(f, pty_state, inner);
}

fn render_pty_grid(f: &mut Frame, pty_state: &SharedPtyState, area: Rect) {
    let Ok(pty) = pty_state.lock() else {
        return;
    };
    let grid = pty.term.grid();
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
            let cwd_str = prompt.cwd.as_deref().unwrap_or(".");
            let wt_tag = if prompt.worktree { " [WT]" } else { "" };
            let title = format!(" Output: #{} [{}]{wt_tag} ", prompt.id, cwd_str);
            let content = match &prompt.status {
                PromptStatus::Pending => "(pending)".to_string(),
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
                    let hint = if prompt.mode == PromptMode::Interactive {
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
                PromptStatus::Completed => {
                    prompt.output.clone().unwrap_or_else(|| "(no output)".to_string())
                }
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
        None => (" Output ".to_string(), "Select a prompt to view output".to_string()),
    };

    // Auto-scroll: compute scroll offset to show the bottom of content
    if app.auto_scroll && matches!(app.mode, AppMode::ViewOutput | AppMode::Interact) {
        if let Some(prompt) = app.selected_prompt() {
            if prompt.status == PromptStatus::Running {
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
        Span::styled(format!(" {msg} "), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
    } else {
        Span::raw("")
    };

    let output_border_color = match app.selected_prompt().map(|p| &p.status) {
        Some(PromptStatus::Running) => Color::Cyan,
        Some(PromptStatus::Idle) => Color::Magenta,
        Some(PromptStatus::Completed) => Color::Green,
        Some(PromptStatus::Failed) => Color::Red,
        Some(PromptStatus::Pending) => Color::Yellow,
        None => Color::Rgb(80, 80, 100),
    };

    let paragraph = Paragraph::new(content)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(output_border_color))
                .title(vec![
                    Span::styled(title, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
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
            (
                format!(" Input (Enter to submit, Esc to cancel){wt_tag} "),
                app.input.clone(),
                Style::default().fg(Color::White),
                if app.worktree_pending { Color::Cyan } else { Color::Green },
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

    let paragraph = Paragraph::new(content)
        .style(style)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title(Span::styled(title, Style::default().fg(border_color))),
        );
    f.render_widget(paragraph, area);

    match app.mode {
        AppMode::Insert => {
            let x = area.x + app.input.len() as u16 + 1;
            let y = area.y + 1;
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
                Style::default().fg(Color::White).bg(Color::DarkGray).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Magenta)
            };
            ListItem::new(Span::styled(path.as_str(), style))
        })
        .collect();

    let list = List::new(items)
        .block(
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
            let preview = app.templates.get(name).map(|t| {
                if t.len() > 40 {
                    format!("{}...", &t[..37])
                } else {
                    t.clone()
                }
            }).unwrap_or_default();

            let style = if i == app.template_suggestion_index {
                Style::default().fg(Color::White).bg(Color::DarkGray).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan)
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(":{name} "), style),
                Span::styled(preview, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
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
            Span::styled("y", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw("/"),
            Span::styled("n", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        ]),
    ];

    let paragraph = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(Span::styled(
                    " Confirm Quit ",
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                )),
        )
        .style(Style::default().bg(Color::Rgb(40, 30, 30)));

    f.render_widget(Clear, popup_area);
    f.render_widget(paragraph, popup_area);
}

fn render_quick_prompts_popup(f: &mut Frame, app: &App, main_area: Rect) {
    let qp = app.keymap.quick_prompt_help();

    // Compute the output panel area (matches render_main_area split)
    let list_pct = if app.list_collapsed { 0 } else { app.list_ratio };
    let output_pct = 100 - list_pct;
    let output_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(list_pct), Constraint::Percentage(output_pct)])
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

fn render_help_bar(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let bindings: Vec<(String, &str)> = match app.mode {
        AppMode::Normal => app.keymap.normal_help(),
        AppMode::Insert => {
            let mut help = app.keymap.insert_help();
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
            spans.push(Span::styled("  ", Style::default().fg(Color::Rgb(60, 60, 60))));
        }
        spans.push(Span::styled(
            key.as_str(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
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

    let paragraph = Paragraph::new(Line::from(spans));
    f.render_widget(paragraph, area);
}
