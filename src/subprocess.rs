//! 加载线程的子进程执行；退出 TUI 时回收正在运行的 Git。
use std::{
    io::Read,
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use command_group::CommandGroup;

pub fn output(command: &mut Command, cancellation: &AtomicBool) -> Result<Output> {
    if cancellation.load(Ordering::Relaxed) {
        bail!("diff loading cancelled");
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .group_spawn()?;
    let mut stdout = child
        .inner()
        .stdout
        .take()
        .context("missing child stdout")?;
    let mut stderr = child
        .inner()
        .stderr
        .take()
        .context("missing child stderr")?;
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let err = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let status = loop {
        if cancellation.load(Ordering::Relaxed) {
            let _ = child.kill();
            break child.wait();
        }
        match child.try_wait() {
            Ok(Some(status)) if out.is_finished() && err.is_finished() => break Ok(status),
            Ok(Some(_)) => thread::sleep(Duration::from_millis(10)),
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(error);
            }
        }
    };
    let stdout = out
        .join()
        .map_err(|_| anyhow::anyhow!("child stdout reader panicked"))??;
    let stderr = err
        .join()
        .map_err(|_| anyhow::anyhow!("child stderr reader panicked"))??;
    if cancellation.load(Ordering::Relaxed) {
        bail!("diff loading cancelled");
    }
    Ok(Output {
        status: status?,
        stdout,
        stderr,
    })
}
