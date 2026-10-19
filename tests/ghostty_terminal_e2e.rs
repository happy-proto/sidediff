#![cfg(feature = "ghostty-e2e")]

use std::{
    io::{Read, Write},
    process::Command,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use libghostty_vt::{
    RenderState, Terminal,
    render::{CellIterator, RowIterator},
    style::StyleColor,
    terminal::{Point, PointCoordinate},
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn ghostty_renders_kat_syntax_color_over_inline_diff_background() -> TestResult {
    run_case(
        "main.rs",
        "fn main() { println!(\"old\"); }\n",
        "fn main() { println!(\"new\"); }\n",
        "\"new\"",
        1,
        |foreground, background| {
            matches!(foreground, StyleColor::Rgb(color) if (color.r, color.g, color.b) == (241, 250, 140))
                && matches!(background, StyleColor::Rgb(color) if (color.r, color.g, color.b) == (50, 92, 71))
        },
    )
}

#[test]
fn ghostty_renders_kat_nested_backgrounds_for_multiple_hosts() -> TestResult {
    for (path, initial, modified, needle) in [
        (
            "justfile",
            "build:\n    echo stable\n",
            "build:\n    echo stable\n# changed\n",
            "echo stable",
        ),
        (
            "README.md",
            "```rust\nfn stable() {}\n```\n",
            "```rust\nfn stable() {}\n```\n\n# changed\n",
            "fn stable",
        ),
    ] {
        run_case(
            path,
            initial,
            modified,
            needle,
            0,
            |_, background| matches!(background, StyleColor::Rgb(color) if (color.r, color.g, color.b) == (64, 67, 84)),
        )?;
    }
    Ok(())
}

fn run_case(
    path: &str,
    initial: &str,
    modified: &str,
    needle: &str,
    cell_offset: u16,
    matches_style: impl Fn(StyleColor, StyleColor) -> bool,
) -> TestResult {
    let repo = TempDir::new()?;
    run_git(repo.path(), &["init", "-q"])?;
    std::fs::write(repo.path().join(path), initial)?;
    run_git(repo.path(), &["add", path])?;
    run_git(
        repo.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "initial",
        ],
    )?;
    std::fs::write(repo.path().join(path), modified)?;

    let cols = 132;
    let rows = 24;
    let pair = native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_sidediff"));
    command.arg("--unstaged");
    command.cwd(repo.path());
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    // 颜色回归显式请求彩色输出，不继承执行环境的无色偏好。
    command.env_remove("NO_COLOR");
    command.env("DFT_DISPLAY", "side-by-side");
    let mut child = pair.slave.spawn_command(command)?;
    drop(pair.slave);

    let (tx, rx) = mpsc::channel();
    let reader_thread = thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        while let Ok(length) = reader.read(&mut buf) {
            if length == 0 || tx.send(buf[..length].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut terminal = Terminal::new(cols, rows)?;
    terminal.set_scrollback_max_lines(Some(100))?;
    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut matched = false;
    let mut observed_style = None;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => {
                terminal.vt_write(&chunk);
                output.extend(chunk);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if let Some((x, y)) = find_screen_text(&terminal, needle)? {
            let cell = terminal.grid_ref(Point::Active(PointCoordinate {
                x: x + cell_offset,
                y,
            }))?;
            let style = cell.style()?;
            observed_style = Some(format!("{:?} / {:?}", style.fg_color, style.bg_color));
            if matches_style(style.fg_color, style.bg_color) {
                matched = true;
                break;
            }
        }
    }

    writer.write_all(b"q")?;
    writer.flush()?;
    let status = child.wait()?;
    drop(writer);
    drop(rx);
    reader_thread.join().expect("PTY reader thread panicked");
    assert!(status.success(), "sidediff exited with {status:?}");
    assert!(
        matched,
        "Ghostty did not show the expected kat color at {path}:{needle}; observed {observed_style:?}; output:\n{}",
        String::from_utf8_lossy(&output)
    );
    Ok(())
}

fn run_git(dir: &std::path::Path, args: &[&str]) -> TestResult {
    let output = Command::new("git").current_dir(dir).args(args).output()?;
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn find_screen_text(
    terminal: &Terminal<'_, '_>,
    needle: &str,
) -> Result<Option<(u16, u32)>, Box<dyn std::error::Error>> {
    let mut state = RenderState::new()?;
    let snapshot = state.update(terminal)?;
    let mut rows = RowIterator::new()?;
    let mut cells = CellIterator::new()?;
    let mut row_iter = rows.update(&snapshot)?;
    let mut y = 0_u32;
    while let Some(row) = row_iter.next() {
        let mut line = String::new();
        let mut cell_offsets = Vec::new();
        let mut cell_iter = cells.update(row)?;
        while let Some(cell) = cell_iter.next() {
            cell_offsets.push(line.len());
            let mut grapheme = String::new();
            cell.graphemes_utf8(&mut grapheme)?;
            if grapheme.is_empty() {
                line.push(' ');
            } else {
                line.push_str(&grapheme);
            }
        }
        if let Some(byte_offset) = line.find(needle) {
            let x = cell_offsets
                .iter()
                .position(|offset| *offset == byte_offset)
                .expect("match must begin at a cell boundary");
            return Ok(Some((x as u16, y)));
        }
        y += 1;
    }
    Ok(None)
}
