mod app;
mod cli;
mod completion;
mod error;
mod git_diff;
mod input;
mod kat_adapter;
mod model;
mod subprocess;
mod ui;

use std::process;

use anyhow::Result;
use clap::Parser;
use cli::Cli;

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        process::exit(1);
    }
}

fn run() -> Result<()> {
    completion::maybe_complete();

    let cli = Cli::parse_from(cli::git_style_args(std::env::args_os()));

    let worktree_root = git_diff::ensure_git_repo()?;
    let tab_width = cli.diff.tab_width;
    let mut request = cli.diff.to_request();
    request.worktree_root = worktree_root;
    let invocation_dir = std::env::current_dir()?;
    if cli.diff.relative.as_deref() == Some("") {
        request.relative_root = Some(git_diff::invocation_prefix()?);
    }
    request.invocation_dir = Some(invocation_dir);
    if std::env::var_os("SIDEDIFF_BENCHMARK_LOAD").is_some() {
        return benchmark_load(&request);
    }
    app::run(request, tab_width)
}

/// 本地实验：测量完整加载路径，计时不包含终端交互和结果指纹计算。
fn benchmark_load(request: &git_diff::DiffRequest) -> Result<()> {
    use std::hash::{Hash, Hasher};
    let start = std::time::Instant::now();
    let files = git_diff::load_diffs(request)?;
    let elapsed = start.elapsed();
    let mut fingerprint = std::collections::hash_map::DefaultHasher::new();
    for file in &files {
        format!(
            "{:?}",
            (
                &file.path,
                &file.old_path,
                file.status,
                &file.metadata_lines,
                &file.hunks,
                &file.lhs_syntax,
                &file.rhs_syntax,
            )
        )
        .hash(&mut fingerprint);
    }
    println!(
        "{}",
        serde_json::json!({
            "files": files.len(),
            "load_ms": elapsed.as_secs_f64() * 1000.0,
            "fingerprint": format!("{:016x}", fingerprint.finish()),
            "profile": difftastic::profile::snapshot(),
        })
    );
    Ok(())
}
