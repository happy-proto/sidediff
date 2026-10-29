use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
};

use crate::model::{AppState, Route, UiLayout, dimmed_style};

pub fn draw(frame: &mut Frame<'_>, app: &mut AppState) {
    let root = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(root);

    let (sidebar_area, diff_area) = split_main_area(rows[1], app);
    let sidebar_block = pane_block(
        if app.is_sidebar_search_active() {
            "Files /"
        } else {
            "Files"
        },
        app.route == Route::FileList,
        false,
        None,
    );
    let diff_block = pane_block(
        "Diff",
        app.route == Route::Diff || (!app.show_sidebar && !app.is_compact_layout()),
        !app.show_sidebar && !app.is_compact_layout(),
        app.current_file()
            .and_then(|file| file.line_diff_reason.as_ref())
            .map(|_| "line diff"),
    );
    let diff_inner = diff_block.inner(diff_area);
    app.update_ui_layout(UiLayout {
        sidebar_inner: sidebar_block.inner(sidebar_area),
        diff_area,
        diff_inner,
    });

    draw_status(frame, rows[0], app);
    if sidebar_area.width > 0 && sidebar_area.height > 0 {
        draw_sidebar(frame, sidebar_area, sidebar_block, app);
    }
    if diff_area.width > 0 && diff_area.height > 0 {
        draw_diff(frame, diff_area, diff_block, app);
    }
    draw_help(frame, rows[2], app);
}

fn split_main_area(area: Rect, app: &AppState) -> (Rect, Rect) {
    if app.is_compact_layout() {
        return match app.route {
            Route::FileList => (area, Rect::default()),
            Route::Diff => (Rect::default(), area),
        };
    }

    if !app.is_sidebar_visible() {
        return (Rect::default(), area);
    }

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(app.sidebar_width),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(area);

    (chunks[0], chunks[2])
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let route = match app.route {
        Route::FileList => "files",
        Route::Diff => "diff",
    };
    let sidebar = if app.is_sidebar_visible() {
        "on"
    } else {
        "off"
    };
    let layout = if app.is_compact_layout() {
        "compact"
    } else {
        "split"
    };

    let mut spans = if let Some(file) = app.current_file() {
        vec![
            Span::styled("sidediff | files: ", dimmed_style()),
            Span::raw(app.files.len().to_string()),
            Span::styled(" | width: ", dimmed_style()),
            Span::raw(app.terminal_width.to_string()),
            Span::styled(" | route: ", dimmed_style()),
            Span::raw(route),
            Span::styled(" | mode: ", dimmed_style()),
            Span::raw(layout),
            Span::styled(" | tabs: ", dimmed_style()),
            Span::raw(app.tab_width.to_string()),
            Span::styled(" | sidebar: ", dimmed_style()),
            Span::raw(sidebar),
            Span::styled(" | current: ", dimmed_style()),
            Span::raw(file.display_name()),
            Span::styled(" | line: ", dimmed_style()),
            Span::raw((app.scroll + 1).to_string()),
        ]
    } else {
        vec![
            Span::styled("sidediff | ", dimmed_style()),
            Span::raw("no matching changes"),
        ]
    };

    if let Some(query) = app.sidebar_search_query() {
        spans.push(Span::styled(" | search: ", dimmed_style()));
        spans.push(Span::raw(format!("/{query}")));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_sidebar(frame: &mut Frame<'_>, area: Rect, block: Block<'static>, app: &AppState) {
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let entries = app.visible_sidebar_entries();
    let visible = entries
        .into_iter()
        .skip(app.sidebar_scroll)
        .take(inner.height as usize)
        .map(sidebar_line)
        .collect::<Vec<_>>();

    frame.render_widget(Paragraph::new(Text::from(visible)), inner);
}

fn sidebar_line(entry: crate::model::SidebarEntry) -> Line<'static> {
    let base_style = if entry.is_selected {
        Style::default()
            .fg(dracula_foreground())
            .bg(dracula_selection())
    } else if entry.is_active_file {
        Style::default().fg(dracula_cyan())
    } else {
        Style::default().fg(dracula_foreground())
    };
    let abbreviated_style = if entry.is_selected {
        Style::default()
            .fg(dracula_purple())
            .bg(dracula_selection())
    } else if entry.is_active_file {
        Style::default().fg(dracula_purple())
    } else {
        Style::default().fg(dracula_comment())
    };

    let mut spans = vec![Span::styled(
        format!("[{}] ", entry.status.short_code()),
        base_style,
    )];

    if !entry.matched_positions.is_empty() {
        spans.extend(highlighted_path_spans(
            &entry.display_path,
            &entry.matched_positions,
            base_style,
        ));
        return Line::from(spans);
    }

    let display_parts = entry.display_path.split('/').collect::<Vec<_>>();
    let original_parts = entry.original_path.split('/').collect::<Vec<_>>();
    let last_index = display_parts.len().saturating_sub(1);

    for (idx, display_part) in display_parts.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled("/", base_style));
        }

        if display_part.is_empty() {
            continue;
        }

        let original_part = original_parts.get(idx).copied().unwrap_or_default();
        let abbreviated = idx < last_index && display_part != &original_part;
        let part_style = if abbreviated {
            abbreviated_style
        } else {
            base_style
        };
        spans.push(Span::styled((*display_part).to_string(), part_style));
    }

    Line::from(spans)
}

fn highlighted_path_spans(
    path: &str,
    matched_positions: &[usize],
    base_style: Style,
) -> Vec<Span<'static>> {
    let matched = matched_positions
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    path.chars()
        .enumerate()
        .map(|(idx, ch)| {
            let style = if matched.contains(&idx) {
                base_style.fg(dracula_yellow())
            } else {
                base_style
            };
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

fn draw_diff(frame: &mut Frame<'_>, area: Rect, block: Block<'static>, app: &mut AppState) {
    let inner = block.inner(area);
    let text: Text<'static> = if let Some(file) = app.current_file() {
        if file.is_empty() {
            Text::from("(empty diff output)")
        } else {
            app.current_rendered_text()
                .map(|text| {
                    Text::from(
                        text.lines
                            .iter()
                            .skip(app.scroll)
                            .take(inner.height as usize)
                            .cloned()
                            .collect::<Vec<_>>(),
                    )
                })
                .unwrap_or_else(|| Text::from("(render unavailable)"))
        }
    } else {
        Text::from("No matching changes")
    };

    frame.render_widget(block, area);
    // 先按 usize 截取可见行，避免 Paragraph 的 u16 滚动上限，同时只复制一屏内容。
    let paragraph = Paragraph::new(text);
    frame.render_widget(paragraph, inner);
}

fn draw_help(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let help = if app.is_sidebar_search_active() {
        "/ search  ↑ ↓ move  Enter open  Esc cancel"
    } else if app.is_compact_layout() && app.route == Route::FileList {
        "Enter open  / search  ↑ ↓ move/page  gg/G top/bottom  q quit"
    } else if app.is_compact_layout() {
        "Esc/q back  ↑ ↓ page  Ctrl-d/u page  gg/G top/bottom  n/N hunk  Ctrl-c twice quit"
    } else if app.is_sidebar_visible() {
        "b sidebar  <- -> focus  ↑ ↓ move/page  Enter open  gg/G top/bottom  n/N hunk  q quit"
    } else {
        "b sidebar  <- -> focus  ↑ ↓ page  Ctrl-d/u page  gg/G top/bottom  n/N hunk  q quit"
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(help, dimmed_style())])),
        area,
    );
}

fn pane_block(
    title: &str,
    focused: bool,
    emphasize: bool,
    annotation: Option<&str>,
) -> Block<'static> {
    let border_style = if focused {
        Style::default().fg(dracula_purple())
    } else if emphasize {
        Style::default().fg(dracula_comment())
    } else {
        dimmed_style()
    };

    let title_style = if focused {
        Style::default().fg(dracula_purple())
    } else {
        dimmed_style()
    };

    let mut title = vec![Span::styled(format!(" {title} "), title_style)];
    if let Some(annotation) = annotation {
        title.push(Span::styled(format!("· {annotation} "), dimmed_style()));
    }

    Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Line::from(title))
}

fn dracula_foreground() -> Color {
    Color::Rgb(248, 248, 242)
}

fn dracula_comment() -> Color {
    Color::Rgb(98, 114, 164)
}

fn dracula_selection() -> Color {
    Color::Rgb(68, 71, 90)
}

fn dracula_purple() -> Color {
    Color::Rgb(189, 147, 249)
}

fn dracula_cyan() -> Color {
    Color::Rgb(139, 233, 253)
}

fn dracula_yellow() -> Color {
    Color::Rgb(241, 250, 140)
}
