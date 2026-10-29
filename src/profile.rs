//! 不进入 TUI 的加载诊断；中断时保留已完成文件及仍在计算的热点。
use crate::git_diff::{self, DiffRequest};
use anyhow::Result;
use std::{
    io::{self, IsTerminal, Write},
    sync::mpsc,
    thread,
    time::Duration,
};

pub fn run(request: DiffRequest, json: bool) -> Result<()> {
    request.progress.enable_profile();
    let progress = request.progress.clone();
    let cancellation = request.cancellation.clone();
    let signal = cancellation.clone();
    ctrlc::set_handler(move || signal.cancel())?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = git_diff::load_diffs(&request);
        let _ = tx.send(result);
    });
    let interactive = io::stderr().is_terminal();
    let result = loop {
        if cancellation.load(std::sync::atomic::Ordering::Relaxed) {
            cancellation.cancel();
            break None;
        }
        match rx.try_recv() {
            Ok(result) => break Some(result),
            Err(mpsc::TryRecvError::Disconnected) => {
                anyhow::bail!("diff loader stopped unexpectedly")
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if interactive {
            let r = progress.report();
            eprint!(
                "\r\x1b[2K{}  {}/{} files  {:.1}s",
                r.stage,
                r.completed_files,
                r.total_files,
                r.elapsed_ms / 1000.
            );
            io::stderr().flush()?;
        }
        thread::sleep(Duration::from_millis(100));
    };
    if interactive {
        eprintln!();
    }
    let report = progress.report();
    if json {
        let mut value = serde_json::to_value(&report)?;
        value["cancelled"] = result.is_none().into();
        if let Some(Err(error)) = &result {
            value["error"] = error.to_string().into();
        }
        serde_json::to_writer_pretty(io::stdout().lock(), &value)?;
        println!();
    } else {
        println!(
            "Loading: {:.3}s  {}/{} files completed, {} omitted{}",
            report.elapsed_ms / 1000.,
            report.completed_files,
            report.total_files,
            report.skipped_files,
            if result.is_none() { " (cancelled)" } else { "" }
        );
        println!("\nStages (wall time):");
        for (stage, ms) in &report.stage_ms {
            println!("  {stage:<28} {ms:>10.2} ms");
        }
        println!("\nPhases (summed worker time; nested phases overlap):");
        let mut phases = report.phase_timings.iter().collect::<Vec<_>>();
        phases.sort_by(|a, b| b.1.total_ms.total_cmp(&a.1.total_ms));
        for (phase, t) in phases {
            println!(
                "  {phase:<28} {:>10.2} ms  max {:>10.2} ms  {} calls",
                t.total_ms, t.max_ms, t.calls
            );
        }
        println!("\nSlowest files (up to 20):");
        for file in report.files.iter().take(20) {
            println!(
                "  {:>10.2} ms  {:>10} bytes  {}",
                file.elapsed_ms,
                file.bytes,
                file.path.escape_debug()
            );
        }
        if !report.active.is_empty() {
            println!("\nUnfinished files:");
        }
        for file in &report.active {
            println!(
                "  {:>10.2} ms  {}  {}",
                file.elapsed_ms,
                file.phase,
                file.path.escape_debug()
            );
        }
    }
    io::stdout().flush()?;
    match result {
        Some(result) => {
            result?;
            Ok(())
        }
        None => std::process::exit(130),
    }
}
