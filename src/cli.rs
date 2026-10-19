use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use clap::{Args, Parser};

use crate::git_diff::{DiffMode, DiffRequest};

const AFTER_HELP: &str = "\
Enable shell completion by adding one of these lines to your shell config:
  fish (~/.config/fish/config.fish):
    COMPLETE=fish sidediff | source
  zsh (~/.zshrc):
    source <(COMPLETE=zsh sidediff)
  bash (~/.bashrc):
    source <(COMPLETE=bash sidediff)";

#[derive(Debug, Parser)]
#[command(
    name = "sidediff",
    version,
    about = "Patch-oriented TUI for Git worktree and revision diffs",
    after_help = AFTER_HELP,
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(flatten)]
    pub diff: DiffArgs,
}

/// Clap 的可选值要求 `=`，在入口保留 Git 的 `-M50%` 等紧贴短选项语法。
pub fn git_style_args(args: impl IntoIterator<Item = OsString>) -> impl Iterator<Item = OsString> {
    let mut options = true;
    args.into_iter().map(move |arg| {
        if arg == "--" {
            options = false;
        }
        if options
            && let Some(text) = arg.to_str()
            && ["-M", "-C", "-B"].iter().any(|flag| text.starts_with(flag))
            && text.len() > 2
            && !text[2..].starts_with('=')
        {
            return format!("{}={}", &text[..2], &text[2..]).into();
        }
        arg
    })
}

#[derive(Debug, Args, Clone)]
pub struct DiffArgs {
    #[arg(long = "tab-width", default_value_t = 4)]
    pub tab_width: usize,

    #[arg(long, visible_alias = "staged")]
    pub cached: bool,

    #[arg(long, conflicts_with = "cached")]
    pub unstaged: bool,

    #[arg(long)]
    pub merge_base: bool,

    #[arg(short = 'U', long = "unified")]
    pub unified: Option<usize>,

    #[arg(short = 'M', long = "find-renames", num_args = 0..=1, default_missing_value = "", require_equals = true)]
    pub find_renames: Option<String>,

    #[arg(short = 'C', long = "find-copies", num_args = 0..=1, default_missing_value = "", require_equals = true)]
    pub find_copies: Option<String>,

    #[arg(long)]
    pub find_copies_harder: bool,

    #[arg(short = 'B', long = "break-rewrites", num_args = 0..=1, default_missing_value = "", require_equals = true)]
    pub break_rewrites: Option<String>,

    #[arg(long = "diff-filter")]
    pub diff_filter: Option<String>,

    #[arg(
        long,
        num_args = 0..=1,
        default_missing_value = "",
        require_equals = true,
        help = "Limit output to the current directory or the specified repository directory",
        add = crate::completion::directory_completer()
    )]
    pub relative: Option<String>,

    #[arg(
        value_name = "REV",
        num_args = 0..=2,
        add = crate::completion::revision_completer()
    )]
    pub revisions: Vec<OsString>,

    #[arg(
        last = true,
        value_name = "PATH",
        add = crate::completion::path_completer()
    )]
    pub paths: Vec<OsString>,
}

impl Default for DiffArgs {
    fn default() -> Self {
        Self {
            tab_width: crate::model::DEFAULT_TAB_WIDTH,
            cached: false,
            unstaged: false,
            merge_base: false,
            unified: None,
            find_renames: None,
            find_copies: None,
            find_copies_harder: false,
            break_rewrites: None,
            diff_filter: None,
            relative: None,
            revisions: Vec::new(),
            paths: Vec::new(),
        }
    }
}

impl DiffArgs {
    pub fn to_request(&self) -> DiffRequest {
        let mut git_args = Vec::new();
        let relative_root = self
            .relative
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(normalize_relative_directory);

        if self.cached {
            git_args.push("--cached".into());
        }
        if self.merge_base {
            git_args.push("--merge-base".into());
        }
        if let Some(unified) = self.unified {
            git_args.push(format!("--unified={unified}").into());
        }
        push_optional_attached_arg(&mut git_args, "-M", self.find_renames.as_deref());
        push_optional_attached_arg(&mut git_args, "-C", self.find_copies.as_deref());
        push_optional_attached_arg(&mut git_args, "-B", self.break_rewrites.as_deref());
        if self.find_copies_harder {
            git_args.push("--find-copies-harder".into());
        }
        if let Some(diff_filter) = &self.diff_filter {
            git_args.push(format!("--diff-filter={diff_filter}").into());
        }
        if let Some(relative) = &self.relative {
            if relative.is_empty() {
                git_args.push("--relative".into());
            } else {
                // Git 将不带尾随 / 的值当成字符串前缀；这里统一限定目录边界。
                let directory = relative_root
                    .as_deref()
                    .expect("explicit relative directory");
                if directory.as_os_str().is_empty() {
                    git_args.push("--no-relative".into());
                } else {
                    let mut argument = OsString::from("--relative=");
                    for component in directory.components() {
                        argument.push(component.as_os_str());
                        argument.push("/");
                    }
                    git_args.push(argument);
                }
            }
        }

        git_args.extend(self.revisions.iter().cloned());
        if !self.paths.is_empty() {
            git_args.push("--".into());
            git_args.extend(self.paths.iter().cloned());
        }

        DiffRequest {
            git_args,
            revisions: self.revisions.clone(),
            difft_context: self.unified,
            worktree_root: PathBuf::new(),
            invocation_dir: None,
            cancellation: Default::default(),
            uses_worktree: !self.cached && self.revisions.len() <= 1,
            relative_root,
            pathspecs: self.paths.clone(),
            mode: if !self.revisions.is_empty() {
                DiffMode::Revisions
            } else if self.cached {
                DiffMode::Cached
            } else if self.unstaged {
                DiffMode::Unstaged
            } else {
                DiffMode::WorktreeAggregate
            },
        }
    }
}

fn normalize_relative_directory(value: &str) -> PathBuf {
    let mut directory = PathBuf::new();
    // 按仓库路径消解 . 和 ..，不解析符号链接。
    for component in Path::new(value).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if directory.file_name().is_some() => {
                directory.pop();
            }
            _ => directory.push(component.as_os_str()),
        }
    }
    directory
}

fn push_optional_attached_arg(target: &mut Vec<OsString>, flag: &str, value: Option<&str>) {
    match value {
        Some("") => target.push(flag.into()),
        Some(value) => target.push(format!("{flag}{value}").into()),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::{Cli, DiffArgs};
    use crate::{git_diff::DiffMode, model::DEFAULT_TAB_WIDTH};

    #[test]
    fn diff_args_build_request() {
        let args = DiffArgs {
            cached: true,
            merge_base: true,
            unified: Some(9),
            find_renames: Some(String::new()),
            relative: Some("src".to_string()),
            revisions: vec!["HEAD~1".into(), "HEAD".into()],
            paths: vec!["src".into()],
            ..DiffArgs::default()
        };

        let request = args.to_request();
        assert_eq!(
            request
                .git_args
                .iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec![
                "--cached",
                "--merge-base",
                "--unified=9",
                "-M",
                "--relative=src/",
                "HEAD~1",
                "HEAD",
                "--",
                "src"
            ]
        );
        assert_eq!(request.difft_context, Some(9));
        assert_eq!(request.mode, DiffMode::Revisions);
    }

    #[test]
    fn default_diff_args_build_aggregate_request() {
        let args = DiffArgs::default();

        let request = args.to_request();
        assert_eq!(args.tab_width, DEFAULT_TAB_WIDTH);
        assert!(request.git_args.is_empty());
        assert!(request.uses_worktree);
        assert_eq!(request.mode, DiffMode::WorktreeAggregate);
    }

    #[test]
    fn unstaged_args_build_unstaged_request() {
        let args = DiffArgs {
            unstaged: true,
            ..DiffArgs::default()
        };

        let request = args.to_request();
        assert!(request.git_args.is_empty());
        assert!(request.uses_worktree);
        assert_eq!(request.mode, DiffMode::Unstaged);
    }

    #[test]
    fn command_factory_builds_cli() {
        let command = super::Cli::command();
        assert_eq!(command.get_name(), "sidediff");
    }

    #[test]
    fn cli_has_no_completion_or_help_subcommands() {
        let command = Cli::command();
        let names = command
            .get_subcommands()
            .map(|subcommand| subcommand.get_name().to_string())
            .collect::<Vec<_>>();
        assert!(!names.contains(&"completion".to_string()));
        assert!(!names.contains(&"help".to_string()));
    }
}
