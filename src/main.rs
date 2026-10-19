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
    app::run(request, tab_width)
}
