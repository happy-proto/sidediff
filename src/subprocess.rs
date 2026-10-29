//! 加载线程的子进程执行；退出 TUI 时回收正在运行的 Git。
use std::{
    io::Read,
    process::{Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use command_group::{CommandGroup, GroupChild};

pub fn output(command: &mut Command, cancellation: &Cancellation) -> Result<Output> {
    if cancellation.load(Ordering::Relaxed) {
        bail!("diff loading cancelled");
    }
    let child = {
        let mut children = cancellation.children.lock().unwrap();
        if cancellation.load(Ordering::Relaxed) {
            bail!("diff loading cancelled");
        }
        let child = Arc::new(Mutex::new(
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .group_spawn()?,
        ));
        children.push(child.clone());
        child
    };
    let _registered = Registered {
        cancellation,
        child: child.clone(),
    };
    let mut process = child.lock().unwrap();
    let mut stdout = process
        .inner()
        .stdout
        .take()
        .context("missing child stdout")?;
    let mut stderr = process
        .inner()
        .stderr
        .take()
        .context("missing child stderr")?;
    drop(process);
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let err = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let status = loop {
        let mut child = child.lock().unwrap();
        if cancellation.load(Ordering::Relaxed) {
            let _ = child.kill();
            break child.wait();
        }
        match child.try_wait() {
            Ok(Some(status)) if out.is_finished() && err.is_finished() => break Ok(status),
            Ok(Some(_)) => {
                drop(child);
                thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                drop(child);
                thread::sleep(Duration::from_millis(10));
            }
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

#[derive(Default, Debug)]
pub struct Cancellation {
    flag: AtomicBool,
    children: Mutex<Vec<Arc<Mutex<GroupChild>>>>,
}
impl Cancellation {
    pub fn load(&self, ordering: Ordering) -> bool {
        self.flag.load(ordering)
    }
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
        for child in self.children.lock().unwrap().iter() {
            let mut child = child.lock().unwrap();
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
struct Registered<'a> {
    cancellation: &'a Cancellation,
    child: Arc<Mutex<GroupChild>>,
}
impl Drop for Registered<'_> {
    fn drop(&mut self) {
        self.cancellation
            .children
            .lock()
            .unwrap()
            .retain(|child| !Arc::ptr_eq(child, &self.child));
    }
}
