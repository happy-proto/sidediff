use std::{
    io,
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use anyhow::Result;
use crossterm::{
    cursor::Show,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, prelude::CrosstermBackend, widgets::Paragraph};

use crate::{
    git_diff,
    input::{Command, InputState},
    model::AppState,
    ui,
};

pub fn run(request: git_diff::DiffRequest, tab_width: usize) -> Result<()> {
    let mut cleanup = TerminalCleanupGuard::new();
    enable_raw_mode()?;
    cleanup.raw_mode = true;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    cleanup.alt_screen = true;
    // 鼠标捕获会妨碍直接选中和复制终端文本，因此暂不启用鼠标事件。

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let Some(files) = load_with_ui(&mut terminal, request)? else {
        return Ok(());
    };
    let size = terminal.size()?;
    let mut app = AppState::with_tab_width(files, size.width, tab_width);

    run_event_loop(&mut terminal, &mut app)
}

fn load_with_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    request: git_diff::DiffRequest,
) -> Result<Option<Vec<crate::model::FileDiff>>> {
    let cancellation = request.cancellation.clone();
    let progress = request.progress.clone();
    wait_for_load(
        cancellation,
        move || git_diff::load_diffs(&request),
        || {
            terminal.draw(|frame| {
                frame.render_widget(Paragraph::new(progress.display()), frame.area())
            })?;
            Ok(!(event::poll(Duration::from_millis(50))?
                && matches!(event::read()?, Event::Key(key) if key.kind == KeyEventKind::Press &&
                (key.code == KeyCode::Char('q') || key.code == KeyCode::Esc ||
                 (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))))))
        },
    )
}

fn wait_for_load<T: Send + 'static>(
    cancellation: Arc<crate::subprocess::Cancellation>,
    load: impl FnOnce() -> Result<T> + Send + 'static,
    mut tick: impl FnMut() -> Result<bool>,
) -> Result<Option<T>> {
    {
        let _cancel = CancelOnDrop(cancellation);
        let (tx, rx) = mpsc::channel();
        // 取消不能等待不可中断的库调用；Git 由 Cancellation 主动回收。
        // 主程序退出后操作系统终止剩余计算线程，线程不访问终端或借用栈数据。
        thread::spawn(move || {
            let _ = tx.send(load());
        });
        loop {
            match rx.try_recv() {
                Ok(result) => return result.map(Some),
                Err(mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("diff loader stopped unexpectedly")
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            if !tick()? {
                return Ok(None);
            }
        }
    }
}

struct CancelOnDrop(Arc<crate::subprocess::Cancellation>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut AppState,
) -> Result<()> {
    let mut input = InputState::default();

    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Resize(width, _) => {
                    app.sync_terminal_width(width);
                }
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    match input.resolve(
                        key,
                        app.is_sidebar_search_active(),
                        app.route == crate::model::Route::FileList,
                    ) {
                        Command::Quit => break,
                        Command::BackOrQuit => {
                            if app.should_quit_on_q() {
                                break;
                            }
                        }
                        Command::ToggleSidebar => app.toggle_sidebar(),
                        Command::CycleFocus => app.cycle_focus(),
                        Command::StartSearch => app.start_sidebar_search(),
                        Command::SearchInput(ch) => app.sidebar_search_push(ch),
                        Command::SearchBackspace => app.sidebar_search_backspace(),
                        Command::CursorLeft => app.move_cursor_left(),
                        Command::CursorRight => app.move_cursor_right(),
                        Command::CursorDown => app.move_cursor_down(),
                        Command::CursorUp => app.move_cursor_up(),
                        Command::PageDown => app.page_down(),
                        Command::PageUp => app.page_up(),
                        Command::HalfPageDown => app.half_page_down(),
                        Command::HalfPageUp => app.half_page_up(),
                        Command::JumpTop => app.jump_top(),
                        Command::JumpBottom => app.jump_bottom(),
                        Command::NextHunk => app.jump_next_hunk(),
                        Command::PrevHunk => app.jump_prev_hunk(),
                        Command::Select => app.select_current(),
                        Command::Escape => app.escape(),
                        Command::ToggleFold => app.toggle_current_fold(),
                        Command::CollapseNode => app.collapse_current_node(),
                        Command::ExpandNode => app.expand_current_node(),
                        Command::CollapseAll => app.collapse_all(),
                        Command::ExpandAll => app.expand_all(),
                        Command::None => {}
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
}

struct TerminalCleanupGuard {
    raw_mode: bool,
    alt_screen: bool,
}

impl TerminalCleanupGuard {
    fn new() -> Self {
        Self {
            raw_mode: false,
            alt_screen: false,
        }
    }
}

impl Drop for TerminalCleanupGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        if self.alt_screen {
            let _ = execute!(stdout, LeaveAlternateScreen);
            let _ = execute!(stdout, Show);
        }
        if self.raw_mode {
            let _ = disable_raw_mode();
        }
    }
}

#[cfg(test)]
mod loading_tests {
    #[test]
    fn cancellation_does_not_wait_for_in_process_computation() {
        let start = std::time::Instant::now();
        let result = super::wait_for_load(
            Default::default(),
            || {
                std::thread::sleep(std::time::Duration::from_secs(2));
                Ok(())
            },
            || Ok(false),
        )
        .unwrap();
        assert!(result.is_none());
        assert!(
            start.elapsed() < std::time::Duration::from_millis(500),
            "cancel waited for computation"
        );
    }
}
