use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use crate::app::{App, AppMode};
use crate::keymap::{NormalAction, ViewAction};
use crate::prompt::{PromptMode, PromptStatus};

pub fn render(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // status bar
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
}

fn render_status_bar(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let (mode_str, mode_color) = match app.mode {
        AppMode::Normal => ("NORMAL", Color::Blue),
        AppMode::Insert => ("INSERT", Color::Green),
        AppMode::ViewOutput => ("VIEW", Color::Yellow),
        AppMode::Interact => ("INTERACT", Color::Magenta),
    };

    let spans = vec![
        Span::raw(" "),
        Span::styled(
            format!(" {mode_str} "),
            Style::default().fg(Color::Black).bg(mode_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" │ ", Style::default().fg(Color::Gray)),
        Span::styled("Workers: ", Style::default().fg(Color::Gray)),
        Span::styled(
            format!("{}", app.active_workers),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("/{}", app.max_workers),
            Style::default().fg(Color::Gray),
        ),
        Span::styled(" │ ", Style::default().fg(Color::Gray)),
        Span::styled("Queue: ", Style::default().fg(Color::Gray)),
        Span::styled(
            format!("{}", app.pending_count()),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" │ ", Style::default().fg(Color::Gray)),
        Span::styled("Done: ", Style::default().fg(Color::Gray)),
        Span::styled(
            format!("{}", app.completed_count()),
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" │ ", Style::default().fg(Color::Gray)),
        Span::styled("Total: ", Style::default().fg(Color::Gray)),
        Span::styled(
            format!("{}", app.prompts.len()),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" │ ", Style::default().fg(Color::Gray)),
        Span::styled(
            format!("[{}]", app.default_mode.label()),
            Style::default().fg(match app.default_mode {
                PromptMode::Interactive => Color::Magenta,
                PromptMode::OneShot => Color::Yellow,
            }).add_modifier(Modifier::BOLD),
        ),
    ];

    let paragraph = Paragraph::new(Line::from(spans))
        .style(Style::default().bg(Color::Rgb(30, 30, 40)))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(mode_color))
                .title(Span::styled(
                    " clhorde ",
                    Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
                )),
        );
    f.render_widget(paragraph, area);
}

fn render_main_area(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    render_prompt_list(f, app, chunks[0]);
    render_output_viewer(f, app, chunks[1]);
}

fn render_prompt_list(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let tick = app.tick;
    let items: Vec<ListItem> = app
        .prompts
        .iter()
        .map(|prompt| {
            let elapsed = prompt
                .elapsed_secs()
                .map(|s| format!(" ({s:.1}s)"))
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

            let truncated = if prompt.text.len() > 30 {
                format!("{}...", &prompt.text[..27])
            } else {
                prompt.text.clone()
            };

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
                    format!("#{} ", prompt.id),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(truncated),
                Span::styled(elapsed, Style::default().fg(Color::DarkGray)),
            ];
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

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(80, 80, 100)))
                .title(Span::styled(
                    " Prompts ",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 40, 60))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_output_viewer(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let (title, content) = match app.selected_prompt() {
        Some(prompt) => {
            let cwd_str = prompt.cwd.as_deref().unwrap_or(".");
            let title = format!(" Output: #{} [{}] ", prompt.id, cwd_str);
            let content = match &prompt.status {
                PromptStatus::Pending => "(pending)".to_string(),
                PromptStatus::Running => {
                    let elapsed = prompt.elapsed_secs().unwrap_or(0.0);
                    match &prompt.output {
                        Some(output) => {
                            format!("Running... ({elapsed:.1}s)\n\n{output}")
                        }
                        None => format!("Running... ({elapsed:.1}s)"),
                    }
                }
                PromptStatus::Idle => {
                    let elapsed = prompt.elapsed_secs().unwrap_or(0.0);
                    let hint = if prompt.mode == PromptMode::Interactive {
                        let key = app.keymap.view_key_hint(ViewAction::Interact);
                        format!(" — press '{}' to interact", key)
                    } else {
                        String::new()
                    };
                    match &prompt.output {
                        Some(output) => {
                            format!("{output}\n\n— Idle ({elapsed:.1}s){hint}")
                        }
                        None => format!("Idle ({elapsed:.1}s){hint}"),
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
                ]),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.scroll_offset, 0));
    f.render_widget(paragraph, area);
}

fn render_input_bar(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let (title, content, style, border_color): (String, String, Style, Color) = match app.mode {
        AppMode::Insert => (
            " Input (Enter to submit, Esc to cancel) ".to_string(),
            app.input.clone(),
            Style::default().fg(Color::White),
            Color::Green,
        ),
        AppMode::Interact => (
            " Interact (Enter to send, Esc to cancel) ".to_string(),
            app.interact_input.clone(),
            Style::default().fg(Color::Cyan),
            Color::Magenta,
        ),
        _ => {
            let key = app.keymap.normal_key_hint(NormalAction::Insert);
            (
                format!(" Input (press '{}' to enter a prompt) ", key),
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

fn render_help_bar(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let bindings: Vec<(String, &str)> = match app.mode {
        AppMode::Normal => app.keymap.normal_help(),
        AppMode::Insert => app.keymap.insert_help(),
        AppMode::ViewOutput => app.keymap.view_help(),
        AppMode::Interact => app.keymap.interact_help(),
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

    let paragraph = Paragraph::new(Line::from(spans));
    f.render_widget(paragraph, area);
}
