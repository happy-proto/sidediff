//! 真实二进制的回归：每个场景都来自已复现的用户可见错误。
use std::{
    io::{Read, Write},
    num::NonZeroU16,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tempfile::TempDir;

struct Repo(TempDir);

impl Repo {
    fn new() -> Self {
        let repo = Self(tempfile::tempdir().unwrap());
        repo.git(&["init", "-q", "--initial-branch=main"]);
        repo
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn git(&self, args: &[&str]) {
        let output = self.git_command(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        command
            .current_dir(self.path())
            .env("GIT_CONFIG_GLOBAL", self.path().join("missing-config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "core.autocrlf=false",
            ])
            .args(args);
        command
    }

    fn commit(&self) {
        self.git(&["add", "."]);
        self.git(&["commit", "-qm", "fixture"]);
    }

    fn dirty() -> Self {
        let repo = Self::new();
        repo.write("base.txt", "before\n");
        repo.commit();
        repo.write("base.txt", "after\n");
        repo
    }

    fn start(&self, args: &[&str]) -> Session {
        Session::start(self.path(), args, 180, &[])
    }
}

struct Session {
    child: Box<dyn Child + Send + Sync>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    _master: Box<dyn MasterPty + Send>,
    rx: mpsc::Receiver<(Vec<u8>, vt100::Screen)>,
    screen: vt100::Screen,
    output: Vec<u8>,
}

#[derive(Default)]
struct CursorReports(Vec<u8>);

impl vt100::Callbacks for CursorReports {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        if i1.is_none() && i2.is_none() && params == [&[6][..]] && c == 'n' {
            let (row, col) = screen.cursor_position();
            write!(self.0, "\x1b[{};{}R", row + 1, col + 1).unwrap();
        }
    }
}

impl Session {
    fn start(cwd: &Path, args: &[&str], cols: u16, env: &[(&str, PathBuf)]) -> Self {
        let executable = std::env::var_os("SIDEDIFF_E2E_BIN")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_sidediff").into());
        let mut command = CommandBuilder::new(executable);
        command.cwd(cwd);
        command.args(args);
        // Windows 上 portable-pty 默认重建注册表环境，显式继承进程环境以保留临时 PATH。
        command.env_clear();
        for (key, value) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("GIT_") || name.starts_with("DFT_") || name == "COMPLETE" {
                continue;
            }
            command.env(key, value);
        }
        command.env("GIT_CONFIG_GLOBAL", cwd.join("missing-config"));
        command.env("GIT_CONFIG_NOSYSTEM", "1");
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        for (key, value) in env {
            command.env(key, value);
        }
        Self::spawn(command, cols)
    }

    fn spawn(command: CommandBuilder, cols: u16) -> Self {
        let rows = NonZeroU16::new(30).unwrap();
        let screen_cols = NonZeroU16::new(cols).expect("terminal width must be nonzero");
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let (tx, rx) = mpsc::channel();
        let input = Arc::clone(&writer);
        thread::spawn(move || {
            // ConPTY 的光标继承查询必须在后台应答，不能依赖测试正在等待屏幕。
            let mut parser =
                vt100::Parser::new_with_callbacks(rows, screen_cols, 0, CursorReports::default());
            let mut buffer = [0; 8192];
            while let Ok(length) = reader.read(&mut buffer) {
                if length == 0 {
                    break;
                }
                parser.process(&buffer[..length]);
                let replies = std::mem::take(&mut parser.callbacks_mut().0);
                if !replies.is_empty() {
                    let mut writer = input.lock().unwrap();
                    if writer
                        .write_all(&replies)
                        .and_then(|_| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
                if tx
                    .send((buffer[..length].to_vec(), parser.screen().clone()))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            child,
            writer,
            _master: pair.master,
            rx,
            screen: vt100::Parser::new(rows, screen_cols, 0).screen().clone(),
            output: Vec::new(),
        }
    }

    fn send(&mut self, keys: &[u8]) {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(keys).unwrap();
        writer.flush().unwrap();
    }

    fn wait_for(&mut self, text: &str) -> String {
        self.wait_for_with_timeout(text, Duration::from_secs(5))
    }

    fn screen_contents(&self) -> String {
        // 保留物理行边界；ConPTY 的自动折行不能改变屏幕坐标。
        self.screen
            .rows(0, self.screen.size().1.get())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn wait_for_with_timeout(&mut self, text: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let received =
                if let Ok((chunk, screen)) = self.rx.recv_timeout(Duration::from_millis(20)) {
                    self.screen = screen;
                    self.output.extend(chunk);
                    true
                } else {
                    false
                };
            let screen = self.screen_contents();
            if screen.contains(text) {
                return screen;
            }
            // 进程退出后先读完排队的终端输出，避免只留下截断的错误前缀。
            if let Some(status) = self.child.try_wait().unwrap()
                && !received
            {
                panic!(
                    "sidediff exited {status:?} before showing {text:?}:\n{}",
                    String::from_utf8_lossy(&self.output)
                );
            }
        }
        panic!(
            "did not show {text:?}:\n{}\nraw:\n{}",
            self.screen_contents(),
            String::from_utf8_lossy(&self.output)
        );
    }

    fn exits_successfully(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "sidediff exited {status:?}");
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("sidediff ignored exit input");
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn terminal_cursor_query_is_answered_before_child_continues() {
    const PROBE: &str = "SIDEDIFF_CURSOR_QUERY_PROBE";
    if std::env::var_os(PROBE).is_some() {
        crossterm::terminal::enable_raw_mode().unwrap();
        let mut stdout = std::io::stdout();
        stdout.write_all(b"\x1b[4;7H\x1b[").unwrap();
        stdout.flush().unwrap();
        stdout.write_all(b"6n").unwrap();
        stdout.flush().unwrap();
        let mut reply = [0; 6];
        std::io::stdin().read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"\x1b[4;7R");
        stdout.write_all(b"CURSOR_QUERY_ANSWERED").unwrap();
        stdout.flush().unwrap();
        let mut quit = [0];
        std::io::stdin().read_exact(&mut quit).unwrap();
        assert_eq!(&quit, b"q");
        crossterm::terminal::disable_raw_mode().unwrap();
        return;
    }

    let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "terminal_cursor_query_is_answered_before_child_continues",
        "--nocapture",
    ]);
    command.env(PROBE, "1");
    let mut session = Session::spawn(command, 80);
    session.wait_for("CURSOR_QUERY_ANSWERED");
    session.send(b"q");
    session.exits_successfully();
}

#[test]
fn line_diff_mode_and_reason_are_visible_without_changing_navigation() {
    let initial = (0..80)
        .map(|i| format!("func before_{i:03}() string {{ return \"BEFORE_MARKER_{i:03}\" }}\n"))
        .collect::<String>();
    let modified = (0..80)
        .map(|i| format!("func after_{i:03}() string {{ return \"AFTER_MARKER_{i:03}\" }}\n"))
        .collect::<String>();
    for (path, env, after, reason) in [
        (
            "main.go",
            vec![("DFT_GRAPH_LIMIT", "0")],
            modified.as_str(),
            Some("graph limit"),
        ),
        (
            "main.go",
            vec![("DFT_BYTE_LIMIT", "1")],
            modified.as_str(),
            Some("byte limit"),
        ),
        (
            "main.go",
            vec![("DFT_PARSE_ERROR_LIMIT", "0")],
            "func broken( { // AFTER_MARKER_000\n",
            Some("parse error limit"),
        ),
        (
            "notes.txt",
            vec![],
            modified.as_str(),
            Some("unavailable for this file type"),
        ),
        ("main.go", vec![], modified.as_str(), None),
    ] {
        let repo = Repo::new();
        repo.write(path, &format!("package fixture\n{initial}"));
        repo.commit();
        repo.write(path, &format!("package fixture\n{after}"));
        let process_env = env
            .iter()
            .map(|(key, value)| (*key, PathBuf::from(value)))
            .collect::<Vec<_>>();
        let mut session = Session::start(repo.path(), &[], 180, &process_env);
        let screen = session.wait_for("AFTER_MARKER_000");
        if let Some(reason) = reason {
            assert!(screen.contains("Diff · line diff"), "{path}: {screen}");
            assert!(
                screen.contains("Line diff:") && screen.contains(reason),
                "{path}: {screen}"
            );
        } else {
            assert!(
                !screen.contains("line diff") && !screen.contains("Line diff:"),
                "{screen}"
            );
        }
        if env.contains(&("DFT_GRAPH_LIMIT", "0")) {
            session.send(b"\tG");
            let bottom = session.wait_for("AFTER_MARKER_079");
            assert!(bottom.contains("Diff · line diff"), "{bottom}");
            assert!(!bottom.contains("Line diff:"), "{bottom}");
            session.send(b"b");
            let hidden = session.wait_for("sidebar: off");
            assert!(hidden.contains("Diff · line diff"), "{hidden}");
        }
    }
}

#[test]
fn default_view_includes_staged_files_before_first_commit() {
    let repo = Repo::new();
    repo.write("staged.txt", "staged\n");
    repo.git(&["add", "."]);
    repo.write("staged.txt", "current worktree\n");
    let mut session = repo.start(&[]);
    session.wait_for("[A] staged.txt");
    session.wait_for("current worktree");
}

#[test]
fn subdirectory_pathspecs_and_bare_relative_preserve_git_semantics() {
    let repo = Repo::new();
    repo.write("outside.txt", "before\n");
    repo.write("sub/inside.txt", "before\n");
    repo.commit();
    repo.write("outside.txt", "outside change\n");
    repo.write("sub/inside.txt", "inside change\n");
    for args in [vec!["--", "inside.txt"], vec!["--relative"]] {
        let mut session = Session::start(&repo.path().join("sub"), &args, 180, &[]);
        let screen = session.wait_for("inside change");
        assert!(screen.contains("files: 1"), "{screen}");
        assert!(!screen.contains("outside.txt"), "{screen}");
    }
}

#[test]
fn relative_scope_excludes_outside_untracked_files() {
    let repo = Repo::dirty();
    repo.write("sub/inside.txt", "inside change\n");
    repo.write("outside.txt", "outside change\n");
    let mut session = repo.start(&["--relative=sub"]);
    let screen = session.wait_for("inside change");
    assert!(screen.contains("files: 1"), "{screen}");
    assert!(!screen.contains("outside.txt"), "{screen}");
}

#[test]
fn status_filter_applies_to_untracked_files() {
    let repo = Repo::dirty();
    repo.write("untracked.txt", "new\n");
    let mut session = repo.start(&["--diff-filter=M"]);
    let screen = session.wait_for("after");
    assert!(screen.contains("files: 1"), "{screen}");
    assert!(!screen.contains("untracked.txt"), "{screen}");
}

#[test]
fn staged_deletion_then_recreation_is_one_aggregate_change() {
    let repo = Repo::dirty();
    repo.git(&["rm", "-f", "base.txt"]);
    repo.write("base.txt", "replacement\n");
    let mut session = repo.start(&[]);
    let screen = session.wait_for("replacement");
    assert!(screen.contains("files: 1"), "{screen}");
    assert!(screen.contains("[M] base.txt"), "{screen}");
    drop(session);
    // 重建为 HEAD 的相同内容时，最终聚合状态应完全没有变化。
    repo.write("base.txt", "before\n");
    repo.start(&[]).wait_for("no matching changes");
}

#[test]
fn unresolved_merge_is_visible_in_cached_and_unstaged_modes() {
    let repo = Repo::new();
    repo.write("base.txt", "before\n");
    repo.commit();
    repo.git(&["checkout", "-qb", "other"]);
    repo.write("base.txt", "other\n");
    repo.commit();
    repo.git(&["checkout", "-q", "main"]);
    repo.write("base.txt", "main\n");
    repo.commit();
    assert!(
        !repo
            .git_command(&["merge", "other"])
            .output()
            .unwrap()
            .status
            .success()
    );
    for mode in ["--unstaged", "--cached"] {
        let mut session = repo.start(&[mode]);
        session.wait_for("[U] base.txt");
        let screen = session.wait_for("Unmerged conflict");
        assert!(screen.contains("files: 1"), "{screen}");
    }
}

#[test]
fn dirty_submodule_does_not_block_other_diffs() {
    let source = Repo::new();
    source.write("child.txt", "child\n");
    source.commit();
    let repo = Repo::dirty();
    repo.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "-q",
        source.path().to_str().unwrap(),
        "vendor",
    ]);
    repo.commit();
    repo.write("vendor/child.txt", "dirty child\n");
    repo.write("base.txt", "parent change\n");
    let mut session = repo.start(&[]);
    session.wait_for("[M] vendor");
    session.wait_for("parent change");
    session.send(b"j");
    session.wait_for("Submodule");
    session.wait_for("dirty");
}

#[test]
fn leading_dash_filename_is_passed_as_a_difft_operand() {
    let repo = Repo::new();
    repo.write("--help", "literal filename\n");
    let mut session = repo.start(&[]);
    session.wait_for("[A] --help");
    session.wait_for("literal filename");
}

#[test]
fn optional_rename_flag_does_not_consume_revision() {
    let repo = Repo::dirty();
    for flag in ["-M", "-C", "-B", "-M50%"] {
        let mut session = repo.start(&[flag, "HEAD"]);
        session.wait_for("after");
    }
}

#[test]
fn search_accepts_all_letters_in_common_filenames() {
    let repo = Repo::new();
    repo.write("kgjGit.txt", "search target\n");
    let mut session = repo.start(&[]);
    session.wait_for("[A] kgjGit.txt");
    session.send(b"/kgjGit");
    session.wait_for("search: /kgjGit");
}

#[test]
fn double_ctrl_c_exits_directly_from_compact_diff() {
    let repo = Repo::dirty();
    let mut session = Session::start(repo.path(), &[], 80, &[]);
    session.wait_for("route: files");
    session.send(b"\r");
    session.wait_for("route: diff");
    session.send(b"\x03\x03");
    session.exits_successfully();
}

#[cfg(unix)]
#[test]
fn executable_mode_change_has_readable_metadata() {
    use std::os::unix::fs::PermissionsExt;
    let repo = Repo::new();
    repo.write("script", "same text\n");
    std::fs::set_permissions(
        repo.path().join("script"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    repo.commit();
    std::fs::set_permissions(
        repo.path().join("script"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let mut session = repo.start(&[]);
    session.wait_for("100644 -> 100755");
}

#[test]
fn final_newline_change_is_visible() {
    let repo = Repo::new();
    repo.write("base.txt", "same text\n");
    repo.commit();
    repo.write("base.txt", "same text");
    let mut session = repo.start(&[]);
    session.wait_for("No newline at end of new file");
}

#[test]
fn end_of_large_added_file_is_reachable() {
    let repo = Repo::new();
    let mut content = (0..70_000)
        .map(|index| format!("line {index}\n"))
        .collect::<String>();
    content.push_str("LAST_LINE_OF_LARGE_FILE\n");
    repo.write("large.txt", &content);
    let mut session = Session::start(repo.path(), &[], 80, &[]);
    session.wait_for("route: files");
    session.send(b"\r");
    session.wait_for("route: diff");
    session.send(b"G");
    session.wait_for("LAST_LINE_OF_LARGE_FILE");
}

#[cfg(unix)]
#[test]
fn repository_directory_can_end_with_a_space() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("trailing ");
    std::fs::create_dir(&path).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&path)
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(path.join("new.txt"), "trailing space repo\n").unwrap();
    let mut session = Session::start(&path, &[], 180, &[]);
    session.wait_for("trailing space repo");
}

#[cfg(unix)]
#[test]
fn external_difft_is_not_invoked() {
    use std::os::unix::fs::PermissionsExt;
    let repo = Repo::dirty();
    let tools = tempfile::tempdir().unwrap();
    let difft = tools.path().join("difft");
    std::fs::write(
        &difft,
        "#!/bin/sh\ntouch \"$SIDEDIFF_TEST_MARKER\"\nexit 99\n",
    )
    .unwrap();
    std::fs::set_permissions(&difft, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![tools.path().to_path_buf()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = PathBuf::from(std::env::join_paths(paths).unwrap());
    let marker = tools.path().join("difft-invoked");
    Session::start(
        repo.path(),
        &[],
        180,
        &[("PATH", path), ("SIDEDIFF_TEST_MARKER", marker.clone())],
    )
    .wait_for("after");
    assert!(
        !marker.exists(),
        "sidediff invoked the external difft executable"
    );
}

#[cfg(unix)]
#[test]
fn loading_can_be_cancelled_while_git_is_running() {
    use std::os::unix::fs::PermissionsExt;
    let repo = Repo::dirty();
    let tools = tempfile::tempdir().unwrap();
    let real_git = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    assert!(real_git.status.success());
    let real_git = String::from_utf8(real_git.stdout).unwrap();
    let git = tools.path().join("git");
    // 仓库探测正常通过，仅 raw diff 枚举挂起，验证取消时回收整个进程组。
    std::fs::write(&git, "#!/bin/sh\nif [ \"$1\" = diff ]; then\nsleep 8 &\ntouch \"$SIDEDIFF_TEST_MARKER\"\nwait\nfi\nexec \"$SIDEDIFF_TEST_REAL_GIT\" \"$@\"\n").unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![tools.path().to_path_buf()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = PathBuf::from(std::env::join_paths(paths).unwrap());
    let marker = tools.path().join("git-started");
    let mut session = Session::start(
        repo.path(),
        &[],
        180,
        &[
            ("PATH", path),
            ("SIDEDIFF_TEST_MARKER", marker.clone()),
            ("SIDEDIFF_TEST_REAL_GIT", real_git.trim().into()),
        ],
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(marker.exists(), "the actual Git diff command did not start");
    session.wait_for("Elapsed:");
    session.wait_for("Enumerating changes");
    session.send(b"\x03");
    session.exits_successfully();
}

#[test]
fn loading_profile_reports_filtered_revisions_as_json() {
    let repo = Repo::new();
    repo.write("first.rs", "fn value() -> u8 { 1 }\n");
    repo.write("second.txt", "old\n");
    repo.commit();
    repo.write("first.rs", "fn value() -> u8 { 2 }\n");
    repo.write("second.txt", "new\n");
    repo.commit();
    let output = Command::new(env!("CARGO_BIN_EXE_sidediff"))
        .current_dir(repo.path())
        .args(["--profile", "--json", "HEAD~1", "HEAD", "--", "first.rs"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["total_files"], 1);
    assert_eq!(report["completed_files"], 1);
    assert_eq!(report["cancelled"], false);
    assert_eq!(report["files"][0]["path"], "first.rs");
    assert!(report["files"][0]["bytes"].as_u64().unwrap() > 0);
    assert!(
        report["phase_timings"]["sidediff.difft"]["total_ms"]
            .as_f64()
            .unwrap()
            >= 0.
    );
    assert!(report["active"].as_array().unwrap().is_empty());
}

#[test]
fn loading_profile_reports_omitted_content_and_errors() {
    let repo = Repo::dirty();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_sidediff"))
            .current_dir(repo.path())
            .args(args)
            .env("SIDEDIFF_MAX_FILES", "0")
            .output()
            .unwrap()
    };
    let output = run(&["--profile", "--json"]);
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["completed_files"], 1);
    assert_eq!(report["skipped_files"], 1);
    assert_eq!(report["files"][0]["skipped"], true);
    let error = run(&["--profile", "--json", "DOES_NOT_EXIST"]);
    assert!(!error.status.success());
    let report: serde_json::Value = serde_json::from_slice(&error.stdout).unwrap();
    assert!(report["error"].is_string());
}

#[cfg(unix)]
#[test]
fn loading_profile_interrupt_returns_partial_json_and_reaps_git() {
    use std::os::unix::fs::PermissionsExt;
    let repo = Repo::dirty();
    let tools = tempfile::tempdir().unwrap();
    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let git = tools.path().join("git");
    let pid_file = tools.path().join("git.pid");
    std::fs::write(&git, "#!/bin/sh\nif [ \"$1\" = diff ]; then\necho $$ > \"$SIDEDIFF_TEST_PID\"\nsleep 15 &\nwait\nfi\nexec \"$SIDEDIFF_TEST_REAL_GIT\" \"$@\"\n").unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![tools.path().to_owned()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_sidediff"))
        .current_dir(repo.path())
        .args(["--profile", "--json"])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env(
            "SIDEDIFF_TEST_REAL_GIT",
            String::from_utf8(real_git.stdout).unwrap().trim(),
        )
        .env("SIDEDIFF_TEST_PID", &pid_file)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pid_file.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if !pid_file.exists() {
        let _ = child.kill();
        panic!("Git did not start");
    }
    assert!(
        Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > Duration::from_secs(3) {
            let _ = child.kill();
            panic!("profile ignored Ctrl-C");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["cancelled"], true);
    assert_eq!(report["stage"], "Enumerating changes");
    let pid = std::fs::read_to_string(pid_file).unwrap();
    assert!(
        !Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success(),
        "Git was left running"
    );
}

#[test]
fn subdirectory_default_view_includes_untracked_files_outside_cwd() {
    let repo = Repo::dirty();
    repo.write("outside.txt", "outside untracked\n");
    repo.write("sub/inside.txt", "inside untracked\n");
    let mut session = Session::start(&repo.path().join("sub"), &[], 180, &[]);
    let screen = session.wait_for("[A] outside.txt");
    assert!(screen.contains("inside.txt"), "{screen}");
    assert!(screen.contains("files: 3"), "{screen}");
}

#[test]
fn revision_ranges_display_committed_snapshots() {
    let repo = Repo::new();
    repo.write("value.txt", "OLD_COMMITTED\n");
    repo.commit();
    repo.write("value.txt", "NEW_COMMITTED\n");
    repo.commit();
    repo.write("value.txt", "UNCOMMITTED_WRONG\n");
    for range in ["HEAD~1..HEAD", "HEAD~1...HEAD", "HEAD^!"] {
        let mut session = repo.start(&[range]);
        let screen = session.wait_for("NEW_COMMITTED");
        assert!(screen.contains("OLD_COMMITTED"), "{screen}");
        assert!(!screen.contains("UNCOMMITTED_WRONG"), "{screen}");
    }
}

#[cfg(unix)]
#[test]
fn committed_symlinks_support_changes_history_and_deletion() {
    use std::os::unix::fs::symlink;
    let repo = Repo::new();
    let link = repo.path().join("link.txt");
    symlink("old-target", &link).unwrap();
    repo.commit();
    std::fs::remove_file(&link).unwrap();
    symlink("new-target", &link).unwrap();
    repo.start(&[]).wait_for("new-target");
    repo.commit();
    repo.start(&["HEAD~1", "HEAD"]).wait_for("old-target");
    std::fs::remove_file(link).unwrap();
    repo.start(&[]).wait_for("new-target");
}

#[cfg(unix)]
#[test]
fn symlink_placeholder_checkout_is_read_as_a_regular_file() {
    use std::os::unix::fs::symlink;
    let repo = Repo::new();
    let link = repo.path().join("link.txt");
    symlink("old-target", &link).unwrap();
    repo.commit();
    repo.git(&["config", "core.symlinks", "false"]);
    std::fs::remove_file(&link).unwrap();
    repo.write("link.txt", "new-placeholder");
    repo.start(&[]).wait_for("new-placeholder");
}

#[test]
fn untracked_nested_repository_does_not_block_other_diffs() {
    let repo = Repo::dirty();
    repo.write("nested/file.txt", "nested content\n");
    repo.git(&["-C", "nested", "init", "-q"]);
    repo.git(&["-C", "nested", "add", "."]);
    repo.git(&["-C", "nested", "commit", "-qm", "nested fixture"]);
    let mut session = repo.start(&[]);
    session.wait_for("after");
    session.send(b"j");
    session.wait_for("Untracked nested repository");
}

fn binary_conflict() -> Repo {
    let repo = Repo::new();
    for (branch, bytes) in [
        ("main", b"\xff\0BASE".as_slice()),
        ("other", b"\xff\0OTHER".as_slice()),
    ] {
        if branch == "other" {
            repo.git(&["checkout", "-qb", "other"]);
        }
        std::fs::write(repo.path().join("image.bin"), bytes).unwrap();
        repo.commit();
    }
    repo.git(&["checkout", "-q", "main"]);
    std::fs::write(repo.path().join("image.bin"), b"\xff\0MAIN").unwrap();
    repo.commit();
    assert!(
        !repo
            .git_command(&["merge", "other"])
            .output()
            .unwrap()
            .status
            .success()
    );
    repo
}

#[test]
fn default_view_includes_unresolved_binary_conflicts() {
    let repo = binary_conflict();
    repo.start(&[]).wait_for("[U] image.bin");
}

#[test]
fn binary_conflict_does_not_require_utf8_worktree_contents() {
    let repo = binary_conflict();
    let mut session = repo.start(&["--unstaged"]);
    session.wait_for("[U] image.bin");
    session.wait_for("Binary conflict");
}

#[test]
fn merge_commit_shorthand_compares_first_parent_to_merge_snapshot() {
    let repo = Repo::new();
    repo.write("base.txt", "BASE\n");
    repo.commit();
    repo.git(&["checkout", "-qb", "other"]);
    repo.write("base.txt", "OTHER_PARENT\n");
    repo.commit();
    repo.git(&["checkout", "-q", "main"]);
    repo.write("base.txt", "FIRST_PARENT\n");
    repo.commit();
    assert!(
        !repo
            .git_command(&["merge", "other"])
            .output()
            .unwrap()
            .status
            .success()
    );
    repo.write("base.txt", "MERGE_RESOLUTION\n");
    repo.commit();
    repo.write("base.txt", "WRONG_WORKTREE\n");
    let mut session = repo.start(&["HEAD^!"]);
    let screen = session.wait_for("MERGE_RESOLUTION");
    assert!(screen.contains("FIRST_PARENT"), "{screen}");
    assert!(!screen.contains("WRONG_WORKTREE"), "{screen}");
    session.wait_for("first parent");
}

#[test]
fn difft_exit_code_environment_does_not_reject_valid_differences() {
    let repo = Repo::dirty();
    Session::start(repo.path(), &[], 180, &[("DFT_EXIT_CODE", "true".into())]).wait_for("after");
}

#[test]
fn content_limit_keeps_file_visible_and_explains_omission() {
    let repo = Repo::dirty();
    let mut session = Session::start(
        repo.path(),
        &[],
        180,
        &[("SIDEDIFF_MAX_FILE_BYTES", "1".into())],
    );
    session.wait_for("Content omitted:");
    session.wait_for("per-side limit (1 bytes)");
}

#[test]
fn invalid_diff_config_restores_terminal_before_reporting_error() {
    let repo = Repo::dirty();
    let mut session = Session::start(
        repo.path(),
        &[],
        180,
        &[("DFT_OVERRIDE_1", "missing-colon".into())],
    );
    session.wait_for("Invalid language override syntax");
    assert!(
        !session.screen.alternate_screen(),
        "error left the terminal in the alternate screen"
    );
}

#[test]
fn library_bulk_attributes_preserve_binary_and_nested_text_rules() {
    let repo = Repo::new();
    repo.write(".gitattributes", "*.txt -diff\n");
    repo.write("sub/.gitattributes", "normal.txt diff\n");
    for path in ["a.txt", "spaced name.txt", "sub/normal.txt"] {
        repo.write(path, "BEFORE_ATTRIBUTE_TEST\n");
    }
    repo.commit();
    for path in ["a.txt", "spaced name.txt", "sub/normal.txt"] {
        repo.write(path, "AFTER_ATTRIBUTE_TEST\n");
    }
    let env = [("SIDEDIFF_DIFF_JOBS", "4".into())];
    Session::start(repo.path(), &[], 180, &env).wait_for("Binary file changed");
    Session::start(repo.path(), &["--", "spaced name.txt"], 180, &env)
        .wait_for("Binary file changed");
    Session::start(repo.path(), &["--", "sub/normal.txt"], 180, &env)
        .wait_for("AFTER_ATTRIBUTE_TEST");
}

#[test]
fn submodule_file_type_changes_preserve_the_text_side() {
    let source = Repo::new();
    source.write("child.txt", "child\n");
    source.commit();
    let output = source.git_command(&["rev-parse", "HEAD"]).output().unwrap();
    let oid = String::from_utf8(output.stdout).unwrap();
    let entry = format!("160000,{},sub", oid.trim());
    let repo = Repo::new();
    repo.git(&["update-index", "--add", "--cacheinfo", &entry]);
    repo.git(&["commit", "-qm", "gitlink fixture"]);
    repo.write("sub", "REPLACEMENT_FILE_TEXT\n");
    repo.start(&[]).wait_for("REPLACEMENT_FILE_TEXT");
    repo.commit();
    repo.start(&["HEAD~1", "HEAD"])
        .wait_for("REPLACEMENT_FILE_TEXT");
    repo.git(&["update-index", "--cacheinfo", &entry]);
    repo.git(&["commit", "-qm", "gitlink again"]);
    repo.start(&["HEAD~1", "HEAD"])
        .wait_for("REPLACEMENT_FILE_TEXT");
}

#[test]
fn exclusion_only_pathspecs_keep_the_full_repository_scope() {
    let repo = Repo::dirty();
    repo.write("outside.txt", "outside untracked\n");
    repo.write("sub/inside.txt", "inside untracked\n");
    repo.write("sub/skip.txt", "excluded\n");
    for exclude in [":!skip.txt", ":^skip.txt", ":(exclude)skip.txt"] {
        let mut session = Session::start(&repo.path().join("sub"), &["--", exclude], 180, &[]);
        let screen = session.wait_for("[A] outside.txt");
        assert!(screen.contains("files: 3"), "{screen}");
        assert!(!screen.contains("skip.txt"), "{screen}");
    }
}

#[test]
fn utf16_text_preserves_unicode_and_final_newlines() {
    for big_endian in [false, true] {
        let repo = Repo::new();
        let write = |text: &str| {
            let bytes = std::iter::once(0xfeff)
                .chain(text.encode_utf16())
                .flat_map(|unit| {
                    if big_endian {
                        unit.to_be_bytes()
                    } else {
                        unit.to_le_bytes()
                    }
                })
                .collect::<Vec<_>>();
            std::fs::write(repo.path().join("unicode.txt"), bytes).unwrap();
        };
        write("before café 中文\n");
        repo.commit();
        write("after café 中文\n");
        let mut session = repo.start(&[]);
        let screen = session.wait_for("after café 中文");
        assert!(screen.contains("before café 中文"), "{screen}");
        assert!(!screen.contains("No newline at end"), "{screen}");
        assert!(!screen.contains('\u{fffd}'), "{screen}");
    }
}

#[test]
fn windows_1252_text_uses_the_same_decoding_as_difft() {
    let repo = Repo::new();
    std::fs::write(
        repo.path().join("latin.txt"),
        b"before caf\xe9 na\xefve \xe9l\xe8ve r\xe9sum\xe9\n",
    )
    .unwrap();
    repo.commit();
    std::fs::write(
        repo.path().join("latin.txt"),
        b"after caf\xe9 na\xefve \xe9l\xe8ve r\xe9sum\xe9\n",
    )
    .unwrap();
    let mut session = repo.start(&[]);
    session.wait_for("after café naïve élève résumé");
}

#[test]
fn selected_file_at_bottom_remains_visible_in_the_sidebar() {
    let repo = Repo::new();
    for index in 0..40 {
        repo.write(
            &format!("f{index:02}.txt"),
            &format!("CONTENT_{index:02}\n"),
        );
    }
    let mut session = repo.start(&[]);
    // 此场景启动 40 次 difft；给批量加载留出预算，导航仍使用常规等待时间。
    session.wait_for_with_timeout("CONTENT_00", Duration::from_secs(20));
    session.send(b"G");
    let screen = session.wait_for("CONTENT_39");
    let sidebar = screen
        .lines()
        .skip(1)
        .map(|line| line.chars().take(15).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(sidebar.contains("f39.txt"), "{screen}");
}

#[test]
fn unborn_default_view_includes_intent_to_add_files() {
    let repo = Repo::new();
    repo.write("intent.txt", "INTENT_TO_ADD_CONTENT\n");
    repo.git(&["add", "-N", "intent.txt"]);
    let mut session = repo.start(&[]);
    session.wait_for("[A] intent.txt");
    session.wait_for("INTENT_TO_ADD_CONTENT");
}

#[test]
fn ignored_recreated_file_still_compares_against_head() {
    let repo = Repo::new();
    repo.write(".gitignore", "tracked.txt\n");
    repo.write("tracked.txt", "OLD_CONTENT\n");
    repo.git(&["add", ".gitignore"]);
    repo.git(&["add", "-f", "tracked.txt"]);
    repo.git(&["commit", "-qm", "ignored tracked fixture"]);
    repo.git(&["rm", "--cached", "tracked.txt"]);
    repo.write("tracked.txt", "NEW_CONTENT\n");
    let mut session = repo.start(&[]);
    session.wait_for("[M] tracked.txt");
    session.wait_for("NEW_CONTENT");
    drop(session);
    repo.write("tracked.txt", "OLD_CONTENT\n");
    repo.start(&[]).wait_for("no matching changes");
}

#[test]
fn rewrite_filter_uses_git_broken_pair_status() {
    let repo = Repo::new();
    repo.write(
        "rewrite.txt",
        &(0..100)
            .map(|i| format!("old line {i}\n"))
            .collect::<String>(),
    );
    repo.commit();
    repo.write(
        "rewrite.txt",
        &(0..100)
            .map(|i| format!("REWRITTEN_CONTENT {}\n", i * 137))
            .collect::<String>(),
    );
    for filter in ["B", "m"] {
        repo.start(&["-B", &format!("--diff-filter={filter}")])
            .wait_for("REWRITTEN_CONTENT");
    }
    for filter in ["M", "b"] {
        repo.start(&["-B", &format!("--diff-filter={filter}")])
            .wait_for("no matching changes");
    }
}

#[test]
fn crlf_lf_only_change_has_readable_metadata() {
    let repo = Repo::new();
    repo.write("endings.txt", "first\r\nsecond\r\n");
    repo.commit();
    repo.write("endings.txt", "first\nsecond\n");
    repo.start(&[]).wait_for("Line endings changed: CRLF -> LF");
}

#[test]
fn ctrl_d_and_u_scroll_half_of_the_diff_viewport() {
    let repo = Repo::new();
    repo.write(
        "rows.txt",
        &(0..100)
            .map(|i| format!("ROW_{i:03}\n"))
            .collect::<String>(),
    );
    let mut session = repo.start(&[]);
    session.wait_for("ROW_000");
    session.send(b"\r");
    session.wait_for("route: diff");
    session.send(b"\x04");
    let screen = session.wait_for("line: 14");
    assert!(!screen.contains("ROW_000"), "{screen}");
    session.send(b"\x15");
    session.wait_for("ROW_000");
}

#[test]
fn formatting_only_change_explains_the_absence_of_syntactic_differences() {
    let repo = Repo::new();
    repo.write("format.rs", "fn main() { println!(\"hello\"); }\n");
    repo.commit();
    repo.write("format.rs", "fn main() {    println!(\"hello\");    }\n");
    repo.start(&[]).wait_for("No syntactic changes");
}

#[test]
fn file_replaced_by_directory_keeps_deleted_file_and_new_children() {
    let repo = Repo::new();
    repo.write("old.txt", "DELETED_FILE_CONTENT\n");
    repo.commit();
    repo.git(&["rm", "old.txt"]);
    repo.write("old.txt/child.txt", "NEW_CHILD_CONTENT\n");
    let mut session = repo.start(&[]);
    let screen = session.wait_for("DELETED_FILE_CONTENT");
    assert!(screen.contains("[D] old.txt"), "{screen}");
    session.send(b"j");
    session.wait_for("NEW_CHILD_CONTENT");
}

#[test]
fn file_replaced_by_nested_repository_keeps_both_entries() {
    let repo = Repo::new();
    repo.write("old.txt", "DELETED_FILE_CONTENT\n");
    repo.commit();
    repo.git(&["rm", "old.txt"]);
    repo.write("old.txt/child.txt", "NESTED_CHILD_CONTENT\n");
    repo.git(&["-C", "old.txt", "init", "-q"]);
    let mut session = repo.start(&[]);
    session.wait_for("DELETED_FILE_CONTENT");
    session.send(b"j");
    session.wait_for("Untracked nested repository");
}

#[test]
fn unborn_staged_file_replaced_by_directory_shows_current_children() {
    let repo = Repo::new();
    repo.write("old.txt", "STAGED_FILE_CONTENT\n");
    repo.git(&["add", "."]);
    std::fs::remove_file(repo.path().join("old.txt")).unwrap();
    repo.write("old.txt/child.txt", "CURRENT_CHILD_CONTENT\n");
    let screen = repo.start(&[]).wait_for("CURRENT_CHILD_CONTENT");
    assert!(!screen.contains("Untracked nested repository"), "{screen}");
}

#[cfg(unix)]
#[test]
fn symlink_conflict_displays_link_payload_without_reading_target() {
    use std::os::unix::fs::symlink;
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("outside-link-target.txt");
    std::fs::write(&target, "ARTIFICIAL_PRIVATE_CONTENT\n").unwrap();
    let repo = Repo::new();
    let link = repo.path().join("link.txt");
    symlink("base-target", &link).unwrap();
    repo.commit();
    repo.git(&["checkout", "-qb", "other"]);
    std::fs::remove_file(&link).unwrap();
    symlink("other-target", &link).unwrap();
    repo.commit();
    repo.git(&["checkout", "main"]);
    std::fs::remove_file(&link).unwrap();
    symlink(&target, &link).unwrap();
    repo.commit();
    let merge = repo.git_command(&["merge", "other"]).output().unwrap();
    assert!(!merge.status.success());
    let screen = repo.start(&[]).wait_for("outside-link-target.txt");
    assert!(screen.contains("[U] link.txt"), "{screen}");
    assert!(!screen.contains("ARTIFICIAL_PRIVATE_CONTENT"), "{screen}");
}

#[cfg(unix)]
#[test]
fn tracked_file_replaced_by_fifo_does_not_block_loading() {
    let repo = Repo::new();
    repo.write("old.txt", "DELETED_FILE_CONTENT\n");
    repo.commit();
    std::fs::remove_file(repo.path().join("old.txt")).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(repo.path().join("old.txt"))
            .status()
            .unwrap()
            .success()
    );
    repo.start(&[]).wait_for("DELETED_FILE_CONTENT");
}

#[cfg(unix)]
#[test]
fn symlink_parent_replacement_does_not_read_external_descendants() {
    use std::os::unix::fs::symlink;
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(
        outside.path().join("old.txt"),
        "ARTIFICIAL_PRIVATE_CONTENT\n",
    )
    .unwrap();
    let repo = Repo::new();
    repo.write("parent/old.txt", "DELETED_FILE_CONTENT\n");
    repo.commit();
    repo.git(&["rm", "-r", "parent"]);
    symlink(outside.path(), repo.path().join("parent")).unwrap();
    let screen = repo.start(&[]).wait_for("[D] parent/old.txt");
    assert!(!screen.contains("ARTIFICIAL_PRIVATE_CONTENT"), "{screen}");
}

#[test]
fn relative_directory_completion_uses_repository_root() {
    let repo = Repo::new();
    repo.write("src/module/file.txt", "before\n");
    repo.commit();
    repo.write("src/module/file.txt", "COMPLETED_SCOPE_CONTENT\n");
    let binary = std::env::var_os("SIDEDIFF_E2E_BIN")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_sidediff").into());
    let mut command = Command::new(binary);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    let output = command
        .current_dir(repo.path().join("src"))
        .env("COMPLETE", "bash")
        .env("_CLAP_COMPLETE_INDEX", "1")
        .env("_CLAP_COMPLETE_COMP_TYPE", "9")
        .env("_CLAP_COMPLETE_SPACE", "false")
        .args(["--", "sidediff", "--relative=src/mo"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let completion = String::from_utf8(output.stdout).unwrap();
    let separator = std::path::MAIN_SEPARATOR;
    let expected = format!("--relative=src{separator}module{separator}");
    let completed_argument = completion
        .lines()
        .find(|line| *line == expected)
        .unwrap_or_else(|| panic!("{completion}"));
    Session::start(&repo.path().join("src"), &[completed_argument], 180, &[])
        .wait_for("COMPLETED_SCOPE_CONTENT");
}

#[test]
fn recreated_rename_source_uses_head_baseline_and_copy_detection() {
    for changed in [false, true] {
        let repo = Repo::new();
        repo.write("old.txt", "ORIGINAL_CONTENT\n");
        repo.commit();
        repo.git(&["mv", "old.txt", "new.txt"]);
        repo.write(
            "old.txt",
            if changed {
                "CHANGED_ORIGINAL\n"
            } else {
                "ORIGINAL_CONTENT\n"
            },
        );
        for args in [&[][..], &["-C"][..], &["-C", "--find-copies-harder"][..]] {
            let copied =
                args.contains(&"-C") && (changed || args.contains(&"--find-copies-harder"));
            let mut session = repo.start(args);
            session.wait_for(if copied { "[C] new.txt" } else { "[A] new.txt" });
            if changed {
                session.wait_for("[M] old.txt");
            } else {
                assert!(!session.screen.contents().contains("[A] old.txt"));
            }
        }
        if changed {
            repo.start(&["--diff-filter=M"])
                .wait_for("CHANGED_ORIGINAL");
        }
    }
}

#[test]
fn difft_check_only_environment_does_not_hide_changes() {
    let repo = Repo::new();
    repo.write("code.rs", "fn main() {}\n");
    repo.commit();
    repo.write("code.rs", "fn main() { println!(\"VISIBLE_CHANGE\"); }\n");
    Session::start(repo.path(), &[], 180, &[("DFT_CHECK_ONLY", "true".into())])
        .wait_for("VISIBLE_CHANGE");
}

#[cfg(unix)]
#[test]
fn invalid_utf8_history_path_does_not_alias_a_replacement_character_file() {
    use std::os::unix::ffi::OsStrExt;
    let repo = Repo::new();
    repo.write("payload", "ORIGINAL_CONTENT\n");
    let output = repo
        .git_command(&["hash-object", "-w", "payload"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let oid = String::from_utf8(output.stdout).unwrap();
    std::fs::remove_file(repo.path().join("payload")).unwrap();
    assert!(
        repo.git_command(&["update-index", "--add", "--cacheinfo", "100644", oid.trim()])
            .arg(std::ffi::OsStr::from_bytes(b"file-\xff.txt"))
            .status()
            .unwrap()
            .success()
    );
    repo.git(&["commit", "-qm", "fixture"]);
    repo.write("file-\u{fffd}.txt", "ORIGINAL_CONTENT\n");
    let mut session = repo.start(&[]);
    session.wait_for("\\xFF");
    session.send(b"j");
    session.wait_for("[A] file-\\u{fffd}.txt");
}

#[test]
fn rewritten_rename_source_preserves_git_break_and_copy_filters() {
    let repo = Repo::new();
    let original = (0..100)
        .map(|i| format!("ORIGINAL LINE {i:03}\n"))
        .collect::<String>();
    repo.write("old.txt", &original);
    repo.commit();
    repo.git(&["mv", "old.txt", "new.txt"]);
    repo.write(
        "old.txt",
        &(0..100)
            .map(|i| format!("COMPLETE REWRITE {i:03}\n"))
            .collect::<String>(),
    );
    let staged = repo
        .git_command(&["ls-files", "--stage", "-z"])
        .output()
        .unwrap();
    assert!(staged.status.success());
    repo.start(&["-B", "--diff-filter=B"])
        .wait_for("COMPLETE REWRITE");
    repo.start(&["-B", "--diff-filter=C"])
        .wait_for("[C] new.txt");
    let after = repo
        .git_command(&["ls-files", "--stage", "-z"])
        .output()
        .unwrap();
    assert!(after.status.success());
    assert_eq!(staged.stdout, after.stdout, "查看差异不能改变暂存状态");
}

#[test]
fn relative_directory_does_not_read_a_similarly_prefixed_path() {
    let repo = Repo::new();
    repo.write("src/a.txt", "INSIDE_ORIGINAL\n");
    repo.write("src/2/b.txt", "UNRELATED_FILE\n");
    repo.write("src2/b.txt", "OUTSIDE_ORIGINAL\n");
    repo.commit();
    repo.write("src/a.txt", "INSIDE_CHANGED\n");
    repo.write("src2/b.txt", "OUTSIDE_CHANGED\n");
    repo.start(&["--relative=src", "--", "src2/b.txt"])
        .wait_for("No matching changes");
    repo.write("src/new.txt", "INSIDE_NEW\n");
    repo.write("src2/new.txt", "OUTSIDE_NEW\n");
    for prefix in [
        "--relative=src",
        "--relative=src/",
        "--relative=./src",
        "--relative=src/../src",
    ] {
        let mut session = repo.start(&[prefix]);
        session.wait_for("INSIDE_CHANGED");
        session.wait_for("files: 2");
        session.send(b"j");
        session.wait_for("INSIDE_NEW");
        assert!(!session.screen.contents().contains("OUTSIDE"));
    }
    let mut session = Session::start(&repo.path().join("src"), &["--relative=."], 180, &[]);
    session.wait_for("files: 4");
    session.send(b"j");
    session.wait_for("OUTSIDE_CHANGED");
}
