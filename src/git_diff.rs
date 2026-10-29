use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, atomic::Ordering},
};

use crate::loading::measure;
use anyhow::{Context, Result, bail};
use gix::bstr::ByteSlice;
use rayon::prelude::*;
use serde::Deserialize;
use tempfile::TempDir;

use crate::{
    error::AppError,
    kat_adapter,
    model::{DiffHighlightKind, DiffHunk, DiffRange, DiffRow, DiffSide, DiffStatus, FileDiff},
};

const NULL_OID: &str = "0000000000000000000000000000000000000000";
#[cfg(windows)]
const DEV_NULL: &str = "NUL";
#[cfg(not(windows))]
const DEV_NULL: &str = "/dev/null";

type AlignedLineKey = (Option<u32>, Option<u32>);
type ChangedLines = HashMap<AlignedLineKey, DifftJsonLine>;
type VisibleLineSets = (HashSet<u32>, HashSet<u32>);
type MergedChunk = (Vec<DifftJsonLine>, i64, VisibleLineSets);

thread_local! {
    // 每个加载线程只初始化一次差异配置，不修改进程环境。
    static DIFFT_LIBRARY: std::result::Result<difftastic::JsonDiff, String> =
        difftastic::JsonDiff::from_env().map_err(|err| err.to_string());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    WorktreeAggregate,
    Unstaged,
    Cached,
    Revisions,
}

#[derive(Debug, Clone)]
pub struct DiffRequest {
    pub git_args: Vec<OsString>,
    pub revisions: Vec<OsString>,
    pub difft_context: Option<usize>,
    pub worktree_root: PathBuf,
    pub invocation_dir: Option<PathBuf>,
    pub cancellation: Arc<crate::subprocess::Cancellation>,
    pub progress: Arc<crate::loading::Progress>,
    pub uses_worktree: bool,
    pub relative_root: Option<PathBuf>,
    pub pathspecs: Vec<OsString>,
    pub mode: DiffMode,
}

impl DiffRequest {
    fn command_dir(&self) -> &Path {
        self.invocation_dir
            .as_deref()
            .unwrap_or(&self.worktree_root)
    }
}

pub fn ensure_git_repo() -> Result<PathBuf> {
    rev_parse_path("--show-toplevel")
}

pub fn invocation_prefix() -> Result<PathBuf> {
    rev_parse_path("--show-prefix")
}

fn rev_parse_path(option: &str) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", option])
        .output()
        .with_context(|| format!("failed to run `git rev-parse {option}`"))?;

    if !output.status.success() {
        return Err(AppError::NotGitRepository.into());
    }

    path_from_git_bytes(output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout))
}

pub fn load_diffs(request: &DiffRequest) -> Result<Vec<FileDiff>> {
    crate::loading::context(&request.progress, None, || {
        let result = load_diffs_with_limits(request, DiffLimits::from_env()?);
        if result.is_ok() {
            request.progress.stage("Ready");
        }
        result
    })
}

#[derive(Clone, Copy)]
struct DiffLimits {
    max_file_bytes: u64,
    max_total_bytes: u64,
    max_files: u64,
}

impl DiffLimits {
    fn from_env() -> Result<Self> {
        fn read(name: &str, default: u64) -> Result<u64> {
            match std::env::var(name) {
                Ok(value) => value
                    .parse()
                    .with_context(|| format!("{name} must be a non-negative integer")),
                Err(std::env::VarError::NotPresent) => Ok(default),
                Err(err) => Err(err).with_context(|| format!("invalid {name}")),
            }
        }
        Ok(Self {
            max_file_bytes: read("SIDEDIFF_MAX_FILE_BYTES", 10 * 1024 * 1024)?,
            max_total_bytes: read("SIDEDIFF_MAX_TOTAL_BYTES", 128 * 1024 * 1024)?,
            max_files: read("SIDEDIFF_MAX_FILES", 5_000)?,
        })
    }
}

fn load_diffs_with_limits(request: &DiffRequest, limits: DiffLimits) -> Result<Vec<FileDiff>> {
    let mut resolved_request = request.clone();
    let mut merge_first_parent = false;
    if request.mode == DiffMode::Revisions && request.uses_worktree {
        // Git 展开 range、^!、^@ 等语法；一个 CLI 参数不一定代表一个端点。
        let output = crate::subprocess::output(
            Command::new("git")
                .current_dir(request.command_dir())
                .args(["rev-parse", "--revs-only", "--end-of-options"])
                .args(&request.revisions),
            &request.cancellation,
        )?;
        if !output.status.success() {
            bail!(
                "failed to resolve revisions: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let revisions = std::str::from_utf8(&output.stdout)?
            .lines()
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        resolved_request.uses_worktree = revisions.len() <= 1;
        if revisions.len() > 2
            && request.revisions.len() == 1
            && request.revisions[0].as_encoded_bytes().ends_with(b"^!")
        {
            // 双栏界面将 merge^! 定义为第一父提交 → merge；不解析多父 combined raw。
            let parent = revisions[1]
                .strip_prefix('^')
                .context("missing first merge parent")?;
            let index = resolved_request
                .git_args
                .iter()
                .position(|arg| arg == &request.revisions[0])
                .context("missing merge revision argument")?;
            resolved_request.git_args.splice(
                index..=index,
                [OsString::from(parent), OsString::from(revisions[0])],
            );
            merge_first_parent = true;
        }
    }
    let request = &resolved_request;
    request.progress.stage("Enumerating changes");
    let mut records = measure("sidediff.enumerate", || load_raw_records(request))?;
    // Git 在未暂存模式可能同时输出同一路径的 U 和 M；保留冲突身份。
    let conflicts = records
        .iter()
        .filter(|record| record.status == DiffStatus::Unmerged)
        .map(|record| record.old_path.clone())
        .collect::<HashSet<_>>();
    records.retain(|record| {
        record.status == DiffStatus::Unmerged || !conflicts.contains(&record.old_path)
    });
    request.progress.plan(records.len());
    request.progress.stage("Opening object database");
    let mut inspection_repo = measure("sidediff.open_gix", || -> Result<_> {
        Ok(gix::discover_with_environment_overrides(
            &request.worktree_root,
        )?)
    })?;
    inspection_repo.objects.ignore_replacements = true;
    let replacements = gix_replacements(&inspection_repo)?;
    // 在读取内容或 batch 预取前准入，按 Git 顺序分配本次加载的预算。
    request.progress.stage("Checking size limits");
    let skipped = measure("sidediff.size_limits", || {
        plan_diff_load(&records, request, &inspection_repo, &replacements, limits)
    })?;
    let paths = records
        .iter()
        .zip(&skipped)
        .filter(|(_, reason)| reason.is_none())
        .map(|(record, _)| display_git_path(record.display_path()))
        .collect::<Vec<_>>();
    let mut engine = DIFFT_LIBRARY.with(|engine| {
        engine
            .as_ref()
            .cloned()
            .map_err(|err| anyhow::anyhow!(err.clone()))
    })?;
    request.progress.stage("Reading attributes");
    let attributes = measure("sidediff.gix_attributes", || {
        gix_binary_attributes(&inspection_repo, &paths, request)
    })?;
    engine.set_binary_attributes(attributes);
    let resources = LoadResources {
        engine,
        replacements,
    };
    let gix_repo = inspection_repo.into_sync();
    request.progress.stage("Computing files");
    let render = |index: usize, repo: &gix::Repository| -> Result<FileDiff> {
        let record = &records[index];
        let path = display_git_path(record.display_path());
        if request.cancellation.load(Ordering::Relaxed) {
            bail!("diff loading cancelled");
        }
        request.progress.start_file(index, path);
        crate::loading::context(&request.progress, Some(index), || {
            let mut rendered = if let Some(reason) = &skipped[index] {
                metadata_file(record, vec![reason.clone()])
            } else {
                render_record(record, request, &resources, repo)?
            };
            if merge_first_parent {
                rendered.metadata_lines.insert(
                    0,
                    "Merge comparison: first parent -> merge commit".to_string(),
                );
            }
            request
                .progress
                .finish_file(index, skipped[index].is_some());
            Ok(rendered)
        })
    };
    let jobs = std::env::var("SIDEDIFF_DIFF_JOBS")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get().min(4)));
    anyhow::ensure!(jobs > 0, "SIDEDIFF_DIFF_JOBS must be positive");
    request.progress.workers(jobs.min(records.len()));
    if jobs == 1 || records.len() <= 1 {
        let repo = gix_read_handle(&gix_repo);
        return (0..records.len())
            .map(|index| render(index, &repo))
            .collect();
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs.min(records.len()))
        .build()?;
    let result = pool.install(|| {
        (0..records.len())
            .into_par_iter()
            .map_init(
                || gix_read_handle(&gix_repo),
                |repo, index| render(index, repo),
            )
            .collect()
    });
    if difftastic::profile::enabled() {
        for stats in pool.broadcast(|_| difftastic::profile::snapshot()) {
            difftastic::profile::merge(stats);
        }
    }
    result
}

fn plan_diff_load(
    records: &[RawRecord],
    request: &DiffRequest,
    repo: &gix::Repository,
    replacements: &HashMap<gix::ObjectId, gix::ObjectId>,
    limits: DiffLimits,
) -> Result<Vec<Option<String>>> {
    let mut files = 0;
    let mut remaining = limits.max_total_bytes;
    records
        .iter()
        .map(|record| {
            if request.cancellation.load(Ordering::Relaxed) {
                bail!("diff loading cancelled");
            }
            if files >= limits.max_files {
                return Ok(Some(format!(
                    "Content omitted: file count limit ({}) reached",
                    limits.max_files
                )));
            }
            let (old, new) = if record.status == DiffStatus::Unmerged {
                // 冲突视图仅展开当前工作区，不读取 index 的冲突 stages。
                let size = if request.uses_worktree {
                    worktree_input_size(request, record.display_path())?.unwrap_or(0)
                } else {
                    0
                };
                (0, size)
            } else {
                (
                    side_input_size(record, true, request, repo, replacements)?,
                    side_input_size(record, false, request, repo, replacements)?,
                )
            };
            if old.max(new) > limits.max_file_bytes {
                return Ok(Some(format!(
                    "Content omitted: file size ({} bytes) exceeds per-side limit ({} bytes)",
                    old.max(new), limits.max_file_bytes
                )));
            }
            let bytes = old.saturating_add(new);
            if bytes > remaining {
                return Ok(Some(format!(
                    "Content omitted: combined old/new contents exceed total input limit ({} bytes)",
                    limits.max_total_bytes
                )));
            }
            files += 1;
            remaining -= bytes;
            Ok(None)
        })
        .collect()
}

fn worktree_input_size(request: &DiffRequest, path: &Path) -> Result<Option<u64>> {
    let path = resolve_worktree_path(request, path);
    let Some(metadata) = worktree_entry_metadata(request, &path) else {
        return Ok(None);
    };
    if metadata.file_type().is_symlink() {
        return Ok(Some(
            fs::read_link(&path)?.as_os_str().as_encoded_bytes().len() as u64,
        ));
    }
    Ok(metadata.is_file().then_some(metadata.len()))
}

fn side_input_size(
    record: &RawRecord,
    old: bool,
    request: &DiffRequest,
    repo: &gix::Repository,
    replacements: &HashMap<gix::ObjectId, gix::ObjectId>,
) -> Result<u64> {
    let (oid, mode, path) = if old {
        (&record.old_oid, &record.old_mode, record.old_path.as_path())
    } else {
        (&record.new_oid, &record.new_mode, record.display_path())
    };
    if matches!(mode.as_str(), "000000" | "160000" | "040000") || (old && oid == NULL_OID) {
        return Ok(0);
    }
    if !old && request.uses_worktree {
        if let Some(size) = worktree_input_size(request, path)? {
            return Ok(size);
        }
        let path = resolve_worktree_path(request, path);
        if worktree_entry_metadata(request, &path).is_some_and(|metadata| !metadata.is_dir()) {
            // render_record 不读取 FIFO 等特殊工作区条目。
            return Ok(0);
        }
    }
    if oid == NULL_OID {
        return Ok(0);
    }
    let oid = resolve_blob_id(gix::ObjectId::from_hex(oid.as_bytes())?, replacements)?;
    let header = repo.find_header(oid)?;
    anyhow::ensure!(
        header.kind() == gix::objs::Kind::Blob,
        "expected blob {oid}"
    );
    Ok(header.size())
}

fn load_raw_records(request: &DiffRequest) -> Result<Vec<RawRecord>> {
    if request.mode == DiffMode::WorktreeAggregate {
        return load_worktree_aggregate_records(request);
    }

    let mut command = Command::new("git");
    command.current_dir(request.command_dir());
    command.arg("diff");
    command.arg("--raw");
    command.arg("-z");
    command.arg("--abbrev=40");
    command.arg("--no-relative");
    command.args(&request.git_args);

    let output = crate::subprocess::output(&mut command, &request.cancellation)
        .context("failed to run `git diff --raw -z`")?;
    if !output.status.success() {
        return Err(AppError::CommandFailed {
            cmd: format_git_command("git diff --raw -z --abbrev=40", &request.git_args),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        }
        .into());
    }

    parse_raw_records(&output.stdout)
}

fn load_worktree_aggregate_records(request: &DiffRequest) -> Result<Vec<RawRecord>> {
    let has_head = head_exists(request)?;
    let mut command = Command::new("git");
    command.current_dir(request.command_dir());
    command.arg("diff");
    command.arg("--raw");
    command.arg("-z");
    command.arg("--abbrev=40");
    command.arg("--no-relative");
    command.arg(if has_head { "HEAD" } else { "--cached" });
    // 聚合后再过滤，避免 staged 删除后重建的文件被提前分成 D 和 A。
    let mut before_paths = true;
    let args = request
        .git_args
        .iter()
        .filter(|arg| {
            if arg.as_os_str() == OsStr::new("--") {
                before_paths = false;
            }
            !before_paths || !arg.to_string_lossy().starts_with("--diff-filter=")
        })
        .collect::<Vec<_>>();
    command.args(&args);

    let output = crate::subprocess::output(&mut command, &request.cancellation)
        .context("failed to run `git diff HEAD --raw -z`")?;
    if !output.status.success() {
        return Err(AppError::CommandFailed {
            cmd: format_git_command("git diff --raw -z --abbrev=40 HEAD", &request.git_args),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        }
        .into());
    }

    let mut records = parse_raw_records(&output.stdout)?;
    // 即使冲突工作区仍等于 HEAD（如二进制保留 ours），index 也可能尚未解决。
    let conflicts = crate::subprocess::output(
        Command::new("git")
            .current_dir(request.command_dir())
            .args([
                "diff",
                "--raw",
                "-z",
                "--abbrev=40",
                "--no-relative",
                "--diff-filter=U",
            ])
            .args(&args),
        &request.cancellation,
    )
    .context("failed to inspect unmerged index entries")?;
    if !conflicts.status.success() {
        bail!(
            "failed to inspect unmerged index entries: {}",
            String::from_utf8_lossy(&conflicts.stderr)
        );
    }
    let conflicts = parse_raw_records(&conflicts.stdout)?;
    let conflict_paths = conflicts
        .iter()
        .map(|record| &record.old_path)
        .collect::<HashSet<_>>();
    let aggregate_index = if has_head {
        create_aggregate_index(request, &records, &conflict_paths)?
    } else {
        None
    };
    let index_path = aggregate_index
        .as_ref()
        .map(|directory| directory.path().join("index"));
    if let Some(index_path) = &index_path {
        command
            .env("GIT_INDEX_FILE", index_path)
            .env("GIT_OPTIONAL_LOCKS", "0");
        let output = crate::subprocess::output(&mut command, &request.cancellation)?;
        if !output.status.success() {
            bail!(
                "failed to compare aggregate worktree: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        records = parse_raw_records(&output.stdout)?;
    }
    records.retain(|record| !conflict_paths.contains(&record.old_path));
    records.extend(conflicts);
    if !has_head {
        let working = crate::subprocess::output(
            Command::new("git")
                .current_dir(request.command_dir())
                .args(["diff", "--raw", "-z", "--abbrev=40", "--no-relative"])
                .args(&args),
            &request.cancellation,
        )
        .context("failed to inspect unborn worktree changes")?;
        if !working.status.success() {
            bail!(
                "failed to inspect unborn worktree changes: {}",
                String::from_utf8_lossy(&working.stderr)
            );
        }
        for record in parse_raw_records(&working.stdout)? {
            if !records
                .iter()
                .any(|existing| existing.display_path() == record.display_path())
            {
                records.push(record);
            }
        }
        records.retain(|record| {
            worktree_entry_metadata(
                request,
                &resolve_worktree_path(request, record.display_path()),
            )
            .is_some_and(|metadata| {
                metadata.is_file()
                    || metadata.file_type().is_symlink()
                    || (record.new_mode == "160000" && metadata.is_dir())
            })
        });
        for record in &mut records {
            if record.new_mode != "160000" {
                record.new_mode =
                    worktree_mode(&resolve_worktree_path(request, record.display_path()))?;
            }
        }
    }

    let mut seen_paths = HashSet::new();
    let mut aggregate = Vec::with_capacity(records.len());
    for record in records {
        let path = resolve_worktree_path(request, record.display_path());
        let metadata = worktree_entry_metadata(request, &path);
        // 目录中的未跟踪内容独立显示，不能与原文件合并为同一个 Git 条目。
        if record.status != DiffStatus::Deleted
            || !metadata.as_ref().is_some_and(|metadata| metadata.is_dir())
        {
            seen_paths.insert(path.clone());
        }
        aggregate.push(record);
    }
    for untracked in load_untracked_records(request, index_path.as_deref())? {
        if seen_paths.insert(resolve_worktree_path(request, untracked.display_path())) {
            aggregate.push(untracked);
        }
    }
    apply_diff_filter(&mut aggregate, request)?;
    Ok(aggregate)
}

fn create_aggregate_index(
    request: &DiffRequest,
    records: &[RawRecord],
    conflict_paths: &HashSet<&PathBuf>,
) -> Result<Option<TempDir>> {
    let restored = records
        .iter()
        .filter(|record| {
            matches!(record.status, DiffStatus::Deleted | DiffStatus::Renamed)
                && !conflict_paths.contains(&record.old_path)
                && worktree_file_exists(request, &resolve_worktree_path(request, &record.old_path))
        })
        .collect::<Vec<_>>();
    if restored.is_empty() {
        return Ok(None);
    }
    // 只修改临时 index，让 Git 计算重写、重命名与复制，保留真实 index 的状态。
    let directory = TempDir::new().context("failed to create aggregate index directory")?;
    let index_path = directory.path().join("index");
    let output = crate::subprocess::output(
        Command::new("git")
            .current_dir(request.command_dir())
            .args(["rev-parse", "--git-path", "index"]),
        &request.cancellation,
    )?;
    if !output.status.success() {
        bail!(
            "failed to locate Git index: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let original = request.command_dir().join(path_from_git_bytes(
        output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout),
    )?);
    match fs::copy(&original, &index_path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let output = crate::subprocess::output(
                Command::new("git")
                    .current_dir(request.command_dir())
                    .env("GIT_INDEX_FILE", &index_path)
                    .args(["-c", "core.splitIndex=false", "read-tree", "HEAD"]),
                &request.cancellation,
            )?;
            if !output.status.success() {
                bail!(
                    "failed to initialize aggregate index: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        Err(error) => return Err(error).context("failed to copy Git index"),
    }
    for record in restored {
        let path = resolve_worktree_path(request, &record.old_path);
        let relative = path.strip_prefix(&request.worktree_root)?;
        // 完整 OID 同时支持 SHA-256 及父仓库中没有对象的 gitlink。
        let output = crate::subprocess::output(
            Command::new("git")
                .current_dir(&request.worktree_root)
                .args(["--literal-pathspecs", "ls-tree", "-z", "HEAD", "--"])
                .arg(relative),
            &request.cancellation,
        )?;
        if !output.status.success() {
            bail!(
                "failed to read aggregate HEAD entry: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let header = output
            .stdout
            .split(|byte| *byte == b'\t')
            .next()
            .context("missing HEAD entry")?;
        let oid = std::str::from_utf8(header)?
            .split_whitespace()
            .nth(2)
            .context("missing HEAD object id")?;
        let output = crate::subprocess::output(
            Command::new("git")
                .current_dir(&request.worktree_root)
                .env("GIT_INDEX_FILE", &index_path)
                .args([
                    "-c",
                    "core.splitIndex=false",
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &record.old_mode,
                    oid,
                ])
                .arg(relative),
            &request.cancellation,
        )?;
        if !output.status.success() {
            bail!(
                "failed to restore aggregate HEAD entry: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    // cacheinfo 没有 stat 信息；先刷新临时缓存，Git 才会把未改动源用于 --find-copies-harder。
    let output = crate::subprocess::output(
        Command::new("git")
            .current_dir(&request.worktree_root)
            .env("GIT_INDEX_FILE", &index_path)
            .args([
                "-c",
                "core.splitIndex=false",
                "update-index",
                "-q",
                "--unmerged",
                "--refresh",
            ]),
        &request.cancellation,
    )?;
    if !output.status.success() {
        bail!(
            "failed to refresh aggregate index: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(Some(directory))
}

fn apply_diff_filter(records: &mut Vec<RawRecord>, request: &DiffRequest) -> Result<()> {
    let Some(filter) = request
        .git_args
        .iter()
        .take_while(|arg| arg.as_os_str() != OsStr::new("--"))
        .find_map(|arg| arg.to_str()?.strip_prefix("--diff-filter="))
    else {
        return Ok(());
    };
    if filter
        .chars()
        .any(|ch| ch != '*' && !"ACDMRTUXBacdmrtuxb".contains(ch))
    {
        bail!("invalid --diff-filter value: {filter}");
    }
    let includes = filter.chars().any(|ch| ch.is_ascii_uppercase());
    let matches = |record: &RawRecord| {
        let code = if record.is_rewrite {
            "B"
        } else {
            record.status.short_code()
        };
        (!includes || filter.contains(code)) && !filter.contains(&code.to_ascii_lowercase())
    };
    if filter.contains('*') {
        if !records.iter().any(matches) {
            records.clear();
        }
    } else {
        records.retain(matches);
    }
    Ok(())
}

fn head_exists(request: &DiffRequest) -> Result<bool> {
    let output = crate::subprocess::output(
        Command::new("git")
            .current_dir(&request.worktree_root)
            .args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]),
        &request.cancellation,
    )
    .context("failed to run `git rev-parse --verify HEAD^{commit}`")?;
    Ok(output.status.success())
}

fn load_untracked_records(
    request: &DiffRequest,
    index_path: Option<&Path>,
) -> Result<Vec<RawRecord>> {
    let mut command = Command::new("git");
    if let Some(index_path) = index_path {
        command
            .env("GIT_INDEX_FILE", index_path)
            .env("GIT_OPTIONAL_LOCKS", "0");
    }
    command.current_dir(request.command_dir());
    command.args([
        "ls-files",
        "--full-name",
        "--others",
        "--exclude-standard",
        "-z",
    ]);
    command.arg("--");
    let defaults = gix::pathspec::Defaults::from_environment(&mut |name| std::env::var_os(name))?;
    if request.pathspecs.iter().all(|path| {
        gix::pathspec::parse(path.as_encoded_bytes(), defaults)
            .is_ok_and(|pattern| pattern.is_excluded())
    }) {
        // 无正向规则时，隐式范围是全仓库；绝对根路径也适用于 literal pathspec 环境。
        command.arg(&request.worktree_root);
    }
    command.args(&request.pathspecs);

    let output = crate::subprocess::output(&mut command, &request.cancellation)
        .context("failed to run `git ls-files --others --exclude-standard -z`")?;
    if !output.status.success() {
        return Err(AppError::CommandFailed {
            cmd: "git ls-files --others --exclude-standard -z".to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        }
        .into());
    }

    let mut records = Vec::new();
    for path in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let path = path_from_git_bytes(path)?;
        let Some(display_path) = strip_relative_root(request, &path) else {
            continue;
        };
        let mode = worktree_mode(&resolve_worktree_path(request, &display_path))?;
        records.push(RawRecord {
            old_mode: "000000".to_string(),
            new_mode: mode,
            old_oid: NULL_OID.to_string(),
            new_oid: NULL_OID.to_string(),
            status: DiffStatus::Created,
            is_rewrite: false,
            old_path: display_path,
            new_path: None,
        });
    }

    Ok(records)
}

fn strip_relative_root(request: &DiffRequest, path: &Path) -> Option<PathBuf> {
    let Some(relative_root) = &request.relative_root else {
        return Some(path.to_path_buf());
    };

    path.strip_prefix(relative_root).ok().map(Path::to_path_buf)
}

fn worktree_mode(path: &Path) -> Result<String> {
    let metadata = path
        .symlink_metadata()
        .with_context(|| format!("failed to read metadata for {}", path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Ok("120000".to_string());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if metadata.is_dir() {
            return Ok("040000".to_string());
        }

        let executable = metadata.permissions().mode() & 0o111 != 0;
        Ok(if executable { "100755" } else { "100644" }.to_string())
    }

    #[cfg(not(unix))]
    {
        if metadata.is_dir() {
            return Ok("040000".to_string());
        }
        Ok("100644".to_string())
    }
}

fn render_record(
    record: &RawRecord,
    request: &DiffRequest,
    resources: &LoadResources,
    repo: &gix::Repository,
) -> Result<FileDiff> {
    if record.status == DiffStatus::Unmerged {
        let mut file = metadata_file(
            record,
            vec!["Unmerged conflict: resolve the index stages with git add".to_string()],
        );
        if request.uses_worktree {
            let path = resolve_worktree_path(request, record.display_path());
            if worktree_file_exists(request, &path) {
                let source = read_worktree_bytes(&path)
                    .with_context(|| format!("failed to read conflict {}", path.display()))?;
                file.metadata_lines
                    .push("Current worktree conflict contents (not a staged snapshot)".to_string());
                if !source.contains(&0)
                    && let Ok(source) = std::str::from_utf8(&source)
                {
                    file.hunks = build_full_side_fallback(
                        &display_git_path(record.display_path()),
                        "Text",
                        DiffStatus::Created,
                        &[],
                        &split_text_lines(source),
                    );
                } else {
                    file.metadata_lines
                        .push("Binary conflict: worktree contents are not displayed".to_string());
                }
            }
        }
        return Ok(file);
    }
    if record.old_mode == "160000" || record.new_mode == "160000" {
        let submodule = render_submodule(record, request)?;
        if (record.old_mode == "160000" || record.old_mode == "000000")
            && (record.new_mode == "160000" || record.new_mode == "000000")
        {
            return Ok(submodule);
        }
        // gitlink 没有可读取的父仓库文件 blob；保留其元数据，另一侧按新增/删除显示。
        let mut text_record = record.clone();
        if record.old_mode == "160000" {
            text_record.old_mode = "000000".to_string();
            text_record.old_oid = NULL_OID.to_string();
            text_record.status = DiffStatus::Created;
        } else {
            text_record.new_mode = "000000".to_string();
            text_record.new_oid = NULL_OID.to_string();
            text_record.status = DiffStatus::Deleted;
        }
        let mut file = render_record(&text_record, request, resources, repo)?;
        file.status = record.status;
        file.metadata_lines.splice(0..0, submodule.metadata_lines);
        file.metadata_lines.insert(
            0,
            format!(
                "File type changed: {} -> {}",
                record.old_mode, record.new_mode
            ),
        );
        return Ok(file);
    }
    if record.new_mode == "040000" {
        return Ok(metadata_file(
            record,
            vec!["Untracked nested repository: contents are not expanded".to_string()],
        ));
    }
    if request.uses_worktree
        && record.new_mode != "000000"
        && worktree_entry_metadata(
            request,
            &resolve_worktree_path(request, record.display_path()),
        )
        .is_some_and(|metadata| {
            !metadata.is_file() && !metadata.is_dir() && !metadata.file_type().is_symlink()
        })
    {
        let mut text_record = record.clone();
        text_record.new_mode = "000000".to_string();
        text_record.new_oid = NULL_OID.to_string();
        text_record.status = DiffStatus::Deleted;
        let mut file = render_record(&text_record, request, resources, repo)?;
        file.status = record.status;
        file.metadata_lines.insert(
            0,
            "Special worktree entry: contents are not read".to_string(),
        );
        return Ok(file);
    }
    render_bytes(
        record,
        request,
        &resources.engine,
        repo,
        &resources.replacements,
    )
}

fn gix_read_handle(shared: &gix::ThreadSafeRepository) -> gix::Repository {
    let mut repo = shared.to_thread_local();
    // 0.87.1 的 replacement_objects_refs_prefix 默认关闭替换，且开关语义反向。
    // 统一禁用底层隐式替换，读取时应用本次加载显式取得的原生 replacement 映射。
    repo.objects.ignore_replacements = true;
    repo
}

fn gix_replacements(repo: &gix::Repository) -> Result<HashMap<gix::ObjectId, gix::ObjectId>> {
    let mut result = HashMap::new();
    if std::env::var_os("GIT_NO_REPLACE_OBJECTS").is_some()
        || repo.config_snapshot().try_boolean("core.useReplaceRefs")? == Some(false)
    {
        return Ok(result);
    }
    let prefix = repo
        .config_snapshot()
        .string("gitoxide.objects.replaceRefBase")
        .unwrap_or_else(|| "refs/replace/".into());
    let references = repo.references()?;
    for reference in references.prefixed(prefix.as_bstr())? {
        let reference = reference.map_err(anyhow::Error::from_boxed)?;
        let name = reference.name().as_bstr();
        if let Some(suffix) = name.strip_prefix(prefix.as_slice())
            && let Ok(source) = gix::ObjectId::from_hex(suffix)
            && let Some(target) = reference.target().try_id()
        {
            result.insert(source, target.to_owned());
        }
    }
    Ok(result)
}

fn resolve_blob_id(
    mut oid: gix::ObjectId,
    replacements: &HashMap<gix::ObjectId, gix::ObjectId>,
) -> Result<gix::ObjectId> {
    // 与 Git 的 MAXREPLACEDEPTH 一致，最多进行 5 轮查找。
    for _ in 0..5 {
        let Some(next) = replacements.get(&oid) else {
            return Ok(oid);
        };
        oid = *next;
    }
    bail!("replace depth too high for {oid}")
}

fn gix_binary_attributes(
    repo: &gix::Repository,
    paths: &[String],
    request: &DiffRequest,
) -> Result<HashMap<String, bool>> {
    if paths.is_empty() {
        return Ok(HashMap::new());
    }
    let index = repo.index_or_empty()?;
    let mut attributes = repo.attributes_only(
        &index,
        gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping,
    )?;
    let mut matches = attributes.selected_attribute_matches(["diff", "binary"]);
    let invocation_prefix = request
        .command_dir()
        .strip_prefix(&request.worktree_root)
        .unwrap_or(Path::new(""));
    let mut result = HashMap::with_capacity(paths.len());
    for path in paths {
        // 与此前 check-attr 的调用目录一致；display path 是其实际接收的字符串。
        let relative = invocation_prefix.join(path);
        attributes
            .at_path(&relative, None)?
            .matching_attributes(&mut matches);
        let binary = matches.iter_selected().any(|m| {
            matches!(
                (m.assignment.name.as_str(), m.assignment.state),
                ("diff", gix::attrs::StateRef::Unset) | ("binary", gix::attrs::StateRef::Set)
            )
        });
        result.insert(path.clone(), binary);
    }
    Ok(result)
}

fn read_worktree_bytes(path: &Path) -> Result<Vec<u8>> {
    if path.symlink_metadata()?.file_type().is_symlink() {
        Ok(fs::read_link(path)?.as_os_str().as_encoded_bytes().to_vec())
    } else {
        fs::read(path).with_context(|| format!("failed to read {}", path.display()))
    }
}

struct ByteSide {
    data: Vec<u8>,
    path: PathBuf,
    missing: bool,
}

fn read_side_bytes(
    record: &RawRecord,
    old: bool,
    request: &DiffRequest,
    repo: &gix::Repository,
    replacements: &HashMap<gix::ObjectId, gix::ObjectId>,
) -> Result<ByteSide> {
    let (oid, mode, path) = if old {
        (&record.old_oid, &record.old_mode, record.old_path.as_path())
    } else {
        (&record.new_oid, &record.new_mode, record.display_path())
    };
    let absent = || ByteSide {
        data: Vec::new(),
        path: DEV_NULL.into(),
        missing: true,
    };
    if mode == "000000" || (old && oid == NULL_OID) {
        return Ok(absent());
    }
    let path = resolve_worktree_path(request, path);
    if !old && request.uses_worktree && worktree_file_exists(request, &path) {
        return Ok(ByteSide {
            data: read_worktree_bytes(&path)?,
            path,
            missing: false,
        });
    }
    if oid == NULL_OID {
        return Ok(absent());
    }
    let blob = measure("sidediff.gix_blob", || -> Result<_> {
        let oid = resolve_blob_id(gix::ObjectId::from_hex(oid.as_bytes())?, replacements)?;
        Ok(repo.find_blob(oid)?)
    })?;
    Ok(ByteSide {
        data: blob.detach().data,
        path,
        missing: false,
    })
}

fn render_bytes(
    record: &RawRecord,
    request: &DiffRequest,
    engine: &difftastic::JsonDiff,
    repo: &gix::Repository,
    replacements: &HashMap<gix::ObjectId, gix::ObjectId>,
) -> Result<FileDiff> {
    if request.cancellation.load(Ordering::Relaxed) {
        bail!("diff loading cancelled");
    }
    let (lhs, rhs) = measure("sidediff.read_bytes", || -> Result<_> {
        Ok((
            read_side_bytes(record, true, request, repo, replacements)?,
            read_side_bytes(record, false, request, repo, replacements)?,
        ))
    })?;
    crate::loading::input_bytes(lhs.data.len() + rhs.data.len());
    let output = measure("sidediff.difft", || {
        engine.diff_slices(
            &display_git_path(record.display_path()),
            &lhs.path,
            &rhs.path,
            &lhs.data,
            &rhs.data,
        )
    })
    .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    if request.cancellation.load(Ordering::Relaxed) {
        bail!("diff loading cancelled");
    }
    let decoded = measure("sidediff.json_decode", || decode_difft_json(&output))?;
    let binary = decoded.language.eq_ignore_ascii_case("binary");
    let (lhs, rhs) = measure("sidediff.decode_text", || -> Result<_> {
        Ok((
            if lhs.missing || binary {
                None
            } else {
                Some(decode_text(&lhs.data)?)
            },
            if rhs.missing || binary {
                None
            } else {
                Some(decode_text(&rhs.data)?)
            },
        ))
    })?;
    measure("sidediff.build_view", || {
        build_file_diff(
            record,
            decoded,
            lhs,
            rhs,
            request.difft_context.unwrap_or(3),
        )
    })
}

fn metadata_file(record: &RawRecord, metadata_lines: Vec<String>) -> FileDiff {
    FileDiff {
        path: display_git_path(record.display_path()),
        old_path: record.rename_from(),
        status: record.status,
        line_diff_reason: None,
        metadata_lines,
        hunks: Vec::new(),
        lhs_syntax: Vec::new(),
        rhs_syntax: Vec::new(),
        lhs_document: None,
        rhs_document: None,
    }
}

fn render_submodule(record: &RawRecord, request: &DiffRequest) -> Result<FileDiff> {
    let mut metadata = vec![
        if record.old_mode == "160000" && record.new_mode == "160000" {
            format!("Submodule: {} -> {}", record.old_oid, record.new_oid)
        } else if record.old_mode == "160000" {
            format!("Submodule removed: {}", record.old_oid)
        } else {
            format!("Submodule added: {}", record.new_oid)
        },
    ];
    if request.uses_worktree && record.new_mode == "160000" {
        let path = resolve_worktree_path(request, record.display_path());
        if path.join(".git").exists() {
            let head = crate::subprocess::output(
                Command::new("git")
                    .current_dir(&path)
                    .args(["rev-parse", "HEAD"]),
                &request.cancellation,
            )?;
            if head.status.success() {
                metadata.push(format!(
                    "Submodule worktree commit: {}",
                    String::from_utf8_lossy(&head.stdout).trim_end()
                ));
            }
            let status = crate::subprocess::output(
                Command::new("git").current_dir(&path).args([
                    "status",
                    "--porcelain",
                    "--untracked-files=normal",
                ]),
                &request.cancellation,
            )?;
            if status.status.success() && !status.stdout.is_empty() {
                metadata.push(
                    "Submodule worktree is dirty (modified or untracked content)".to_string(),
                );
            }
        } else {
            metadata.push("Submodule worktree is not initialized".to_string());
        }
    }
    Ok(metadata_file(record, metadata))
}

fn decode_difft_json(output: &[u8]) -> Result<DifftJsonFile> {
    serde_json::from_slice(output).context("failed to decode difft JSON output")
}

fn decode_text(bytes: &[u8]) -> Result<String> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(text.to_owned());
    }
    if matches!(bytes, [0xff, 0xfe, ..] | [0xfe, 0xff, ..]) {
        let big_endian = bytes.starts_with(&[0xfe, 0xff]);
        let units = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                if big_endian {
                    u16::from_be_bytes(*pair)
                } else {
                    u16::from_le_bytes(*pair)
                }
            })
            .collect::<Vec<_>>();
        // 保留 BOM，与 difft 生成的文本位置保持一致。
        if let Ok(text) = String::from_utf16(&units) {
            return Ok(text);
        }
    }
    let text = String::from_utf8_lossy(bytes);
    let invalid_count = |text: &str| {
        text.chars()
            .take(50_000)
            .filter(|ch| *ch == '\u{fffd}' || *ch == '\0')
            .count()
    };
    if invalid_count(&text) <= 2 {
        return Ok(text.into_owned());
    }
    let (text, _, malformed) = encoding_rs::WINDOWS_1252.decode(bytes);
    if !malformed && invalid_count(&text) <= 1 {
        return Ok(text.into_owned());
    }
    bail!("difft classified an unsupported text encoding as text")
}

fn build_file_diff(
    record: &RawRecord,
    decoded: DifftJsonFile,
    lhs_src: Option<String>,
    rhs_src: Option<String>,
    context: usize,
) -> Result<FileDiff> {
    let path = display_git_path(record.display_path());
    let line_diff_reason = line_diff_reason(&decoded.language);
    let resolved_language = resolved_display_language(&decoded.language, &path);

    let mut metadata_lines = Vec::new();
    if record.old_mode != record.new_mode
        && record.old_mode != "000000"
        && record.new_mode != "000000"
    {
        metadata_lines.push(format!(
            "Mode changed: {} -> {}",
            record.old_mode, record.new_mode
        ));
    }
    match record.status {
        DiffStatus::Renamed => metadata_lines.push(format!(
            "Renamed from {} to {}",
            display_git_path(&record.old_path),
            path
        )),
        DiffStatus::Copied => metadata_lines.push(format!(
            "Copied from {} to {}",
            display_git_path(&record.old_path),
            path
        )),
        DiffStatus::TypeChanged => metadata_lines.push("File type changed".to_string()),
        _ => {}
    }

    if decoded.language.eq_ignore_ascii_case("binary") {
        metadata_lines.push(match record.status {
            DiffStatus::Created => "Binary file created".to_string(),
            DiffStatus::Deleted => "Binary file deleted".to_string(),
            _ => "Binary file changed".to_string(),
        });

        return Ok(FileDiff {
            path,
            old_path: record.rename_from(),
            status: record.status,
            line_diff_reason: None,
            metadata_lines,
            hunks: Vec::new(),
            lhs_syntax: Vec::new(),
            rhs_syntax: Vec::new(),
            lhs_document: None,
            rhs_document: None,
        });
    }

    let old_missing_newline = lhs_src
        .as_ref()
        .is_some_and(|text| !text.is_empty() && !text.ends_with('\n'));
    let new_missing_newline = rhs_src
        .as_ref()
        .is_some_and(|text| !text.is_empty() && !text.ends_with('\n'));
    if old_missing_newline {
        metadata_lines.push("No newline at end of old file".to_string());
    }
    if new_missing_newline {
        metadata_lines.push("No newline at end of new file".to_string());
    }
    if let (Some(lhs), Some(rhs)) = (lhs_src.as_deref(), rhs_src.as_deref()) {
        let endings = |text: &str| {
            let crlf = text.matches("\r\n").count();
            let lf = text.bytes().filter(|byte| *byte == b'\n').count();
            match (crlf, lf) {
                (_, 0) => None,
                (0, _) => Some("LF"),
                (crlf, lf) if crlf == lf => Some("CRLF"),
                _ => Some("mixed CRLF/LF"),
            }
        };
        if let (Some(old), Some(new)) = (endings(lhs), endings(rhs))
            && old != new
        {
            metadata_lines.push(format!("Line endings changed: {old} -> {new}"));
        }
    }

    let lhs_lines = split_text_lines(lhs_src.as_deref().unwrap_or_default());
    let rhs_lines = split_text_lines(rhs_src.as_deref().unwrap_or_default());
    let hunks = match (decoded.aligned_lines.as_deref(), decoded.chunks.as_deref()) {
        (Some(aligned_lines), Some(chunks)) if !chunks.is_empty() => build_hunks(
            &path,
            &resolved_language,
            aligned_lines,
            chunks.to_vec(),
            &lhs_lines,
            &rhs_lines,
            context,
        )?,
        (Some(aligned_lines), _) => build_aligned_only_hunks(
            &path,
            &resolved_language,
            aligned_lines,
            &lhs_lines,
            &rhs_lines,
            context,
        ),
        (_, Some(chunks)) if !chunks.is_empty() => build_chunk_only_hunks(
            &path,
            &resolved_language,
            chunks,
            &lhs_lines,
            &rhs_lines,
            context,
        ),
        _ => build_full_side_fallback(
            &path,
            &resolved_language,
            record.status,
            &lhs_lines,
            &rhs_lines,
        ),
    };

    if hunks.is_empty() && metadata_lines.is_empty() {
        metadata_lines.push(
            "No syntactic changes reported by difft (Git reports a changed file)".to_string(),
        );
    }

    let lhs_document = measure("kat.prepare", || {
        kat_adapter::prepare_document(
            &display_git_path(&record.old_path),
            lhs_src.as_deref().unwrap_or_default(),
        )
    })?;
    let rhs_document = measure("kat.prepare", || {
        kat_adapter::prepare_document(&path, rhs_src.as_deref().unwrap_or_default())
    })?;
    let lhs_syntax = measure("kat.split_spans", || {
        kat_adapter::split_spans(
            lhs_src.as_deref().unwrap_or_default(),
            lhs_document.as_ref(),
        )
    });
    let rhs_syntax = measure("kat.split_spans", || {
        kat_adapter::split_spans(
            rhs_src.as_deref().unwrap_or_default(),
            rhs_document.as_ref(),
        )
    });

    Ok(FileDiff {
        path,
        old_path: record.rename_from(),
        status: record.status,
        line_diff_reason,
        metadata_lines,
        hunks,
        lhs_syntax,
        rhs_syntax,
        lhs_document,
        rhs_document,
    })
}

fn line_diff_reason(language: &str) -> Option<String> {
    if language == "Text" {
        return Some("structural comparison is unavailable for this file type".to_string());
    }
    let reason = language.strip_prefix("Text (")?.strip_suffix(')')?;
    Some(if reason == "exceeded DFT_GRAPH_LIMIT" {
        "structural comparison exceeded the graph limit".to_string()
    } else if let Some(size) = reason.strip_suffix(" exceeded DFT_BYTE_LIMIT") {
        format!("file size {size} exceeded the structural comparison byte limit")
    } else {
        reason.replace("DFT_PARSE_ERROR_LIMIT", "the parse error limit")
    })
}

fn resolved_display_language(difft_language: &str, path: &str) -> String {
    if difft_language.eq_ignore_ascii_case("binary") {
        return difft_language.to_string();
    }

    if let Some(language) = kat_adapter::display_language_name(Some(difft_language), path) {
        return language;
    }

    if let Some(language) = kat_adapter::display_language_name(None, path) {
        return language;
    }

    difft_language.to_string()
}

fn split_text_lines(source: &str) -> Vec<&str> {
    source.lines().collect()
}

fn build_aligned_only_hunks(
    _path: &str,
    language: &str,
    aligned_lines: &[(Option<u32>, Option<u32>)],
    lhs_lines: &[&str],
    rhs_lines: &[&str],
    context: usize,
) -> Vec<DiffHunk> {
    if aligned_lines.is_empty() {
        return Vec::new();
    }

    let end = aligned_lines.len().min(context.max(1));
    vec![DiffHunk {
        title: Some(chunk_title(1, 1, language)),
        rows: rows_from_aligned_pairs(&aligned_lines[..end], None, lhs_lines, rhs_lines),
    }]
}

fn build_hunks(
    _path: &str,
    language: &str,
    aligned_lines: &[(Option<u32>, Option<u32>)],
    chunks: Vec<Vec<DifftJsonLine>>,
    lhs_lines: &[&str],
    rhs_lines: &[&str],
    context: usize,
) -> Result<Vec<DiffHunk>> {
    let mut windows = Vec::new();
    let aligned_index = aligned_line_index(aligned_lines);

    for chunk in chunks {
        let mut changed = HashMap::new();
        let mut indexes = Vec::new();

        for line in chunk {
            let key = (
                line.lhs.as_ref().map(|side| side.line_number),
                line.rhs.as_ref().map(|side| side.line_number),
            );
            changed.insert(key, line);
            let idx = *aligned_index
                .get(&key)
                .with_context(|| format!("failed to locate aligned line for {:?}", key))?;
            indexes.push(idx);
        }

        if indexes.is_empty() {
            continue;
        }

        indexes.sort_unstable();
        let start = indexes[0].saturating_sub(context);
        let end = (indexes[indexes.len() - 1] + context + 1).min(aligned_lines.len());
        windows.push(HunkWindow {
            start,
            end,
            changed,
        });
    }

    if windows.is_empty() {
        return Ok(Vec::new());
    }

    windows.sort_by_key(|window| window.start);

    let mut merged: Vec<HunkWindow> = Vec::new();
    for mut window in windows {
        if let Some(last) = merged.last_mut()
            && window.start <= last.end
        {
            last.end = last.end.max(window.end);
            last.changed.extend(window.changed.drain());
            continue;
        }
        merged.push(window);
    }

    let total_hunks = merged.len();
    let mut hunks = Vec::with_capacity(total_hunks);
    for window in merged {
        let rows = rows_from_aligned_pairs(
            &aligned_lines[window.start..window.end],
            Some(&window.changed),
            lhs_lines,
            rhs_lines,
        );
        let chunk_index = hunks.len() + 1;
        hunks.push(DiffHunk {
            title: Some(chunk_title(chunk_index, total_hunks, language)),
            rows,
        });
    }

    Ok(hunks)
}

fn rows_from_aligned_pairs(
    aligned_pairs: &[(Option<u32>, Option<u32>)],
    changed: Option<&ChangedLines>,
    lhs_lines: &[&str],
    rhs_lines: &[&str],
) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    let mut previous = None;

    for pair in aligned_pairs {
        if let Some((prev_lhs, prev_rhs)) = previous {
            if let (None, Some(prev_rhs), None, Some(curr_rhs)) =
                (prev_lhs, prev_rhs, pair.0, pair.1)
            {
                for line_number in (prev_rhs + 1)..curr_rhs {
                    rows.push(DiffRow::new(
                        None,
                        Some(DiffSide {
                            line_number: line_number as usize + 1,
                            text: line_text(rhs_lines, line_number),
                            diff_ranges: Vec::new(),
                        }),
                    ));
                }
            }
            if let (Some(prev_lhs), None, Some(curr_lhs), None) =
                (prev_lhs, prev_rhs, pair.0, pair.1)
            {
                for line_number in (prev_lhs + 1)..curr_lhs {
                    rows.push(DiffRow::new(
                        Some(DiffSide {
                            line_number: line_number as usize + 1,
                            text: line_text(lhs_lines, line_number),
                            diff_ranges: Vec::new(),
                        }),
                        None,
                    ));
                }
            }
        }

        let lhs = pair.0.map(|line_number| DiffSide {
            line_number: line_number as usize + 1,
            text: line_text(lhs_lines, line_number),
            diff_ranges: changed
                .and_then(|changed| changed.get(pair))
                .and_then(|line| line.lhs.as_ref())
                .map(|side| to_diff_ranges(&side.changes))
                .unwrap_or_default(),
        });
        let rhs = pair.1.map(|line_number| DiffSide {
            line_number: line_number as usize + 1,
            text: line_text(rhs_lines, line_number),
            diff_ranges: changed
                .and_then(|changed| changed.get(pair))
                .and_then(|line| line.rhs.as_ref())
                .map(|side| to_diff_ranges(&side.changes))
                .unwrap_or_default(),
        });
        rows.push(DiffRow::new(lhs, rhs));
        previous = Some(*pair);
    }

    rows
}

fn aligned_line_index(
    aligned_lines: &[(Option<u32>, Option<u32>)],
) -> HashMap<AlignedLineKey, usize> {
    aligned_lines
        .iter()
        .copied()
        .enumerate()
        .map(|(idx, pair)| (pair, idx))
        .collect()
}

fn build_chunk_only_hunks(
    _path: &str,
    language: &str,
    chunks: &[Vec<DifftJsonLine>],
    lhs_lines: &[&str],
    rhs_lines: &[&str],
    context: usize,
) -> Vec<DiffHunk> {
    let mut line_offset = 0_i64;
    let max_lhs_line = lhs_lines.len().saturating_sub(1) as u32;
    let max_rhs_line = rhs_lines.len().saturating_sub(1) as u32;
    let mut merged_chunks: Vec<MergedChunk> = Vec::new();
    for chunk in chunks {
        let visible_lines = chunk_context_line_sets(chunk, max_lhs_line, max_rhs_line, context);

        if let Some((previous_chunk, _, previous_lines)) = merged_chunks.last_mut()
            && (!previous_lines.0.is_disjoint(&visible_lines.0)
                || !previous_lines.1.is_disjoint(&visible_lines.1))
        {
            previous_chunk.extend(chunk.iter().cloned());
            previous_lines.0.extend(visible_lines.0);
            previous_lines.1.extend(visible_lines.1);
        } else {
            merged_chunks.push((chunk.to_vec(), line_offset, visible_lines));
        }

        line_offset += chunk_line_delta(chunk);
    }

    let mut hunks = merged_chunks
        .into_iter()
        .map(|(chunk, chunk_line_offset, _)| DiffHunk {
            title: None,
            rows: expand_chunk_rows(&chunk, lhs_lines, rhs_lines, context, chunk_line_offset),
        })
        .collect::<Vec<_>>();

    let total = hunks.len();
    for (idx, hunk) in hunks.iter_mut().enumerate() {
        hunk.title = Some(chunk_title(idx + 1, total, language));
    }

    hunks
}

fn chunk_context_line_sets(
    chunk: &[DifftJsonLine],
    max_lhs_line: u32,
    max_rhs_line: u32,
    context: usize,
) -> VisibleLineSets {
    let mut lhs = HashSet::new();
    let mut rhs = HashSet::new();
    let lines = extract_chunk_lines(chunk);

    for (lhs_line, rhs_line) in &lines {
        if let Some(lhs_line) = lhs_line {
            lhs.insert(*lhs_line);
        }
        if let Some(rhs_line) = rhs_line {
            rhs.insert(*rhs_line);
        }
    }

    if let Some(first_lhs) = lines.iter().find_map(|(lhs, _)| *lhs) {
        for line in pad_before(first_lhs, context) {
            lhs.insert(line);
        }
    }
    if let Some(last_lhs) = lines.iter().rev().find_map(|(lhs, _)| *lhs) {
        for line in pad_after(last_lhs, max_lhs_line, context) {
            lhs.insert(line);
        }
    }
    if let Some(first_rhs) = lines.iter().find_map(|(_, rhs)| *rhs) {
        for line in pad_before(first_rhs, context) {
            rhs.insert(line);
        }
    }
    if let Some(last_rhs) = lines.iter().rev().find_map(|(_, rhs)| *rhs) {
        for line in pad_after(last_rhs, max_rhs_line, context) {
            rhs.insert(line);
        }
    }

    (lhs, rhs)
}

fn extract_chunk_lines(chunk: &[DifftJsonLine]) -> Vec<(Option<u32>, Option<u32>)> {
    let mut relevant = Vec::new();
    let mut prev_lhs = None;
    let mut prev_rhs = None;

    for line in chunk {
        let current = (
            line.lhs.as_ref().map(|side| side.line_number),
            line.rhs.as_ref().map(|side| side.line_number),
        );
        relevant.extend(fill_between(prev_lhs, current.0, prev_rhs, current.1));
        relevant.push(current);
        prev_lhs = current.0.or(prev_lhs);
        prev_rhs = current.1.or(prev_rhs);
    }

    relevant
}

fn fill_between(
    prev_lhs: Option<u32>,
    next_lhs: Option<u32>,
    prev_rhs: Option<u32>,
    next_rhs: Option<u32>,
) -> Vec<(Option<u32>, Option<u32>)> {
    let lhs_lines = if let (Some(prev_lhs), Some(next_lhs)) = (prev_lhs, next_lhs) {
        if prev_lhs + 1 < next_lhs {
            (prev_lhs + 1..next_lhs).map(Some).collect::<Vec<_>>()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    let rhs_lines = if let (Some(prev_rhs), Some(next_rhs)) = (prev_rhs, next_rhs) {
        if prev_rhs + 1 < next_rhs {
            (prev_rhs + 1..next_rhs).map(Some).collect::<Vec<_>>()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    zip_pad_shorter(&lhs_lines, &rhs_lines)
}

fn zip_pad_shorter<T: Copy>(lhs: &[Option<T>], rhs: &[Option<T>]) -> Vec<(Option<T>, Option<T>)> {
    let max_len = lhs.len().max(rhs.len());
    (0..max_len)
        .map(|idx| {
            (
                lhs.get(idx).copied().flatten(),
                rhs.get(idx).copied().flatten(),
            )
        })
        .collect()
}

fn pad_before(line: u32, context: usize) -> Vec<u32> {
    let mut result = Vec::new();
    let mut current = line;
    for _ in 0..context + 1 {
        if current == 0 {
            break;
        }
        current -= 1;
        result.push(current);
    }
    result.reverse();
    result
}

fn pad_after(line: u32, max_line: u32, context: usize) -> Vec<u32> {
    let mut result = Vec::new();
    let mut current = line;
    for _ in 0..context + 1 {
        if current >= max_line {
            break;
        }
        current += 1;
        result.push(current);
    }
    result
}

fn build_full_side_fallback(
    _path: &str,
    language: &str,
    status: DiffStatus,
    lhs_lines: &[&str],
    rhs_lines: &[&str],
) -> Vec<DiffHunk> {
    match status {
        DiffStatus::Created => vec![DiffHunk {
            title: Some(chunk_title(1, 1, language)),
            rows: rhs_lines
                .iter()
                .enumerate()
                .map(|(idx, line)| {
                    DiffRow::new(
                        None,
                        Some(DiffSide {
                            line_number: idx + 1,
                            text: (*line).to_string(),
                            diff_ranges: vec![DiffRange {
                                start: 0,
                                end: line.len(),
                                kind: DiffHighlightKind::Normal,
                            }],
                        }),
                    )
                })
                .collect(),
        }],
        DiffStatus::Deleted => vec![DiffHunk {
            title: Some(chunk_title(1, 1, language)),
            rows: lhs_lines
                .iter()
                .enumerate()
                .map(|(idx, line)| {
                    DiffRow::new(
                        Some(DiffSide {
                            line_number: idx + 1,
                            text: (*line).to_string(),
                            diff_ranges: vec![DiffRange {
                                start: 0,
                                end: line.len(),
                                kind: DiffHighlightKind::Normal,
                            }],
                        }),
                        None,
                    )
                })
                .collect(),
        }],
        _ => Vec::new(),
    }
}

fn expand_chunk_rows(
    chunk: &[DifftJsonLine],
    lhs_lines: &[&str],
    rhs_lines: &[&str],
    context: usize,
    initial_line_offset: i64,
) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    let mut last_lhs = None;
    let mut last_rhs = None;
    let mut line_offset = initial_line_offset;

    for (idx, line) in chunk.iter().enumerate() {
        let current_lhs = line.lhs.as_ref().map(|side| side.line_number);
        let current_rhs = line.rhs.as_ref().map(|side| side.line_number);

        if idx == 0 {
            push_prefix_context(
                &mut rows,
                lhs_lines,
                rhs_lines,
                current_lhs.or(current_rhs).unwrap_or(0),
                context,
                line_offset,
            );
        }

        if let (Some(prev_lhs), Some(prev_rhs), Some(curr_lhs), Some(curr_rhs)) =
            (last_lhs, last_rhs, current_lhs, current_rhs)
        {
            let lhs_gap = curr_lhs.saturating_sub(prev_lhs + 1);
            let rhs_gap = curr_rhs.saturating_sub(prev_rhs + 1);
            let shared_gap = lhs_gap.min(rhs_gap);
            push_shared_gap(
                &mut rows,
                lhs_lines,
                rhs_lines,
                prev_lhs + 1,
                prev_rhs + 1,
                shared_gap,
                context,
            );
            if lhs_gap > shared_gap {
                push_single_sided_gap(
                    &mut rows,
                    lhs_lines,
                    prev_lhs + 1 + shared_gap,
                    lhs_gap - shared_gap,
                    context,
                    true,
                );
            }
            if rhs_gap > shared_gap {
                push_single_sided_gap(
                    &mut rows,
                    rhs_lines,
                    prev_rhs + 1 + shared_gap,
                    rhs_gap - shared_gap,
                    context,
                    false,
                );
            }
        }
        if last_lhs.is_none()
            && current_lhs.is_none()
            && let (Some(prev_rhs), Some(curr_rhs)) = (last_rhs, current_rhs)
        {
            push_single_sided_gap(
                &mut rows,
                rhs_lines,
                prev_rhs + 1,
                curr_rhs.saturating_sub(prev_rhs + 1),
                context,
                false,
            );
        }
        if last_rhs.is_none()
            && current_rhs.is_none()
            && let (Some(prev_lhs), Some(curr_lhs)) = (last_lhs, current_lhs)
        {
            push_single_sided_gap(
                &mut rows,
                lhs_lines,
                prev_lhs + 1,
                curr_lhs.saturating_sub(prev_lhs + 1),
                context,
                true,
            );
        }

        rows.push(DiffRow::new(
            line.lhs.as_ref().map(|side| DiffSide {
                line_number: side.line_number as usize + 1,
                text: line_text(lhs_lines, side.line_number),
                diff_ranges: to_diff_ranges(&side.changes),
            }),
            line.rhs.as_ref().map(|side| DiffSide {
                line_number: side.line_number as usize + 1,
                text: line_text(rhs_lines, side.line_number),
                diff_ranges: to_diff_ranges(&side.changes),
            }),
        ));

        match (current_lhs, current_rhs) {
            (Some(lhs), Some(rhs)) => line_offset = rhs as i64 - lhs as i64,
            (Some(_), None) => line_offset -= 1,
            (None, Some(_)) => line_offset += 1,
            (None, None) => {}
        }

        last_lhs = current_lhs.or(last_lhs);
        last_rhs = current_rhs.or(last_rhs);
    }

    let trailing = context as i64;
    let mut rhs_cursor = last_rhs.map(|rhs| rhs as i64);
    let mut lhs_cursor = last_lhs.map(|lhs| lhs as i64);
    if rhs_cursor.is_none() {
        rhs_cursor = lhs_cursor.map(|lhs| lhs + line_offset);
    }
    if lhs_cursor.is_none() {
        lhs_cursor = rhs_cursor.map(|rhs| rhs - line_offset);
    }

    push_trailing_context(
        &mut rows, lhs_lines, rhs_lines, lhs_cursor, rhs_cursor, trailing,
    );

    rows
}

fn push_prefix_context(
    rows: &mut Vec<DiffRow>,
    lhs_lines: &[&str],
    rhs_lines: &[&str],
    anchor: u32,
    context: usize,
    line_offset: i64,
) {
    let prefix_start = anchor.saturating_sub(context as u32);
    for line_number in prefix_start..anchor {
        let lhs_idx = line_number as i64 - line_offset;
        let rhs_idx = line_number as i64;
        let lhs = line_from_index(lhs_lines, lhs_idx);
        let rhs = line_from_index(rhs_lines, rhs_idx);
        if lhs.is_none() && rhs.is_none() {
            continue;
        }
        rows.push(DiffRow::new(lhs, rhs));
    }
}

fn push_shared_gap(
    rows: &mut Vec<DiffRow>,
    lhs_lines: &[&str],
    rhs_lines: &[&str],
    lhs_start: u32,
    rhs_start: u32,
    gap: u32,
    context: usize,
) {
    push_gap_window(rows, gap, context, |rows, offset| {
        rows.push(DiffRow::new(
            Some(make_plain_side(lhs_lines, lhs_start + offset)),
            Some(make_plain_side(rhs_lines, rhs_start + offset)),
        ));
    });
}

fn push_single_sided_gap(
    rows: &mut Vec<DiffRow>,
    lines: &[&str],
    start: u32,
    gap: u32,
    context: usize,
    is_lhs: bool,
) {
    push_gap_window(rows, gap, context, |rows, offset| {
        let side = Some(make_plain_side(lines, start + offset));
        if is_lhs {
            rows.push(DiffRow::new(side, None));
        } else {
            rows.push(DiffRow::new(None, side));
        }
    });
}

fn push_gap_window(
    rows: &mut Vec<DiffRow>,
    gap: u32,
    context: usize,
    mut push_row: impl FnMut(&mut Vec<DiffRow>, u32),
) {
    if gap == 0 {
        return;
    }

    let context = context as u32;
    if gap > context * 2 {
        for offset in 0..context {
            push_row(rows, offset);
        }
        rows.push(DiffRow::omitted());
        for offset in (gap - context)..gap {
            push_row(rows, offset);
        }
        return;
    }

    for offset in 0..gap {
        push_row(rows, offset);
    }
}

fn push_trailing_context(
    rows: &mut Vec<DiffRow>,
    lhs_lines: &[&str],
    rhs_lines: &[&str],
    lhs_cursor: Option<i64>,
    rhs_cursor: Option<i64>,
    trailing: i64,
) {
    if let (Some(lhs), Some(rhs)) = (lhs_cursor, rhs_cursor) {
        for offset in 1..=trailing {
            let lhs = line_from_index(lhs_lines, lhs + offset);
            let rhs = line_from_index(rhs_lines, rhs + offset);
            if lhs.is_none() && rhs.is_none() {
                break;
            }
            rows.push(DiffRow::new(lhs, rhs));
        }
    }
}

fn make_plain_side(lines: &[&str], line_number: u32) -> DiffSide {
    DiffSide {
        line_number: line_number as usize + 1,
        text: line_text(lines, line_number),
        diff_ranges: Vec::new(),
    }
}

fn line_from_index(lines: &[&str], line_number: i64) -> Option<DiffSide> {
    if line_number < 0 {
        return None;
    }
    let idx = line_number as usize;
    lines.get(idx).map(|line| DiffSide {
        line_number: idx + 1,
        text: (*line).to_string(),
        diff_ranges: Vec::new(),
    })
}

fn chunk_line_delta(chunk: &[DifftJsonLine]) -> i64 {
    chunk
        .iter()
        .fold(0_i64, |delta, line| match (&line.lhs, &line.rhs) {
            (Some(_), None) => delta - 1,
            (None, Some(_)) => delta + 1,
            _ => delta,
        })
}

fn chunk_title(index: usize, total: usize, language: &str) -> String {
    format!("{index}/{total} --- {language}")
}

fn line_text(lines: &[&str], line_number: u32) -> String {
    lines
        .get(line_number as usize)
        .copied()
        .unwrap_or("")
        .to_string()
}

fn to_diff_ranges(changes: &[DifftJsonChange]) -> Vec<DiffRange> {
    changes
        .iter()
        .map(|change| DiffRange {
            start: change.start as usize,
            end: change.end as usize,
            kind: change.highlight,
        })
        .collect()
}

#[derive(Debug)]
struct HunkWindow {
    start: usize,
    end: usize,
    changed: HashMap<(Option<u32>, Option<u32>), DifftJsonLine>,
}

#[derive(Debug, Deserialize)]
struct DifftJsonFile {
    #[serde(default)]
    aligned_lines: Option<Vec<(Option<u32>, Option<u32>)>>,
    #[serde(default)]
    chunks: Option<Vec<Vec<DifftJsonLine>>>,
    language: String,
}

#[derive(Debug, Deserialize, Clone)]
struct DifftJsonLine {
    #[serde(default)]
    lhs: Option<DifftJsonSide>,
    #[serde(default)]
    rhs: Option<DifftJsonSide>,
}

#[derive(Debug, Deserialize, Clone)]
struct DifftJsonSide {
    line_number: u32,
    #[serde(default)]
    changes: Vec<DifftJsonChange>,
}

#[derive(Debug, Deserialize, Clone)]
struct DifftJsonChange {
    start: u32,
    end: u32,
    highlight: DiffHighlightKind,
}

fn parse_raw_records(bytes: &[u8]) -> Result<Vec<RawRecord>> {
    let mut idx = 0;
    let mut records = Vec::new();

    while idx < bytes.len() {
        let Some(header_end) = bytes[idx..].iter().position(|b| *b == 0) else {
            bail!("unterminated raw diff header");
        };
        let header = &bytes[idx..idx + header_end];
        idx += header_end + 1;

        if header.is_empty() {
            continue;
        }

        let header = String::from_utf8_lossy(header);
        let parsed = parse_raw_header(&header)?;

        let old_path = take_nul_field(bytes, &mut idx)?;
        let new_path = if matches!(parsed.status, DiffStatus::Renamed | DiffStatus::Copied) {
            Some(take_nul_field(bytes, &mut idx)?)
        } else {
            None
        };

        records.push(RawRecord {
            old_mode: parsed.old_mode,
            new_mode: parsed.new_mode,
            old_oid: parsed.old_oid,
            new_oid: parsed.new_oid,
            status: parsed.status,
            is_rewrite: parsed.is_rewrite,
            old_path,
            new_path,
        });
    }

    Ok(records)
}

fn parse_raw_header(header: &str) -> Result<ParsedHeader> {
    let trimmed = header
        .strip_prefix(':')
        .context("invalid raw diff header: missing `:` prefix")?;
    let mut parts = trimmed.split_whitespace();

    let old_mode = parts.next().context("missing old mode")?.to_string();
    let new_mode = parts.next().context("missing new mode")?.to_string();
    let old_oid = parts.next().context("missing old oid")?.to_string();
    let new_oid = parts.next().context("missing new oid")?.to_string();
    let status_token = parts.next().context("missing status")?;
    if parts.next().is_some() {
        bail!("unexpected extra fields in raw diff header `{header}`");
    }

    let (status, similarity) = parse_status_token(status_token)?;
    Ok(ParsedHeader {
        old_mode,
        new_mode,
        old_oid,
        new_oid,
        status,
        is_rewrite: status == DiffStatus::Modified && similarity.is_some_and(|score| score > 0),
    })
}

fn parse_status_token(token: &str) -> Result<(DiffStatus, Option<u32>)> {
    let mut chars = token.chars();
    let kind = chars.next().context("empty status token")?;
    let similarity = chars.as_str();

    let status = match kind {
        'A' => DiffStatus::Created,
        'D' => DiffStatus::Deleted,
        'M' => DiffStatus::Modified,
        'R' => DiffStatus::Renamed,
        'C' => DiffStatus::Copied,
        'T' => DiffStatus::TypeChanged,
        'U' => DiffStatus::Unmerged,
        other => bail!("unsupported raw diff status `{other}`"),
    };

    let similarity = if similarity.is_empty() {
        None
    } else {
        Some(
            similarity
                .parse::<u32>()
                .with_context(|| format!("invalid similarity score `{similarity}`"))?,
        )
    };

    Ok((status, similarity))
}

fn take_nul_field(bytes: &[u8], idx: &mut usize) -> Result<PathBuf> {
    let Some(end) = bytes[*idx..].iter().position(|b| *b == 0) else {
        bail!("unterminated raw diff path field");
    };
    let field = path_from_git_bytes(&bytes[*idx..*idx + end])?;
    *idx += end + 1;
    Ok(field)
}

fn path_from_git_bytes(bytes: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(PathBuf::from(OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        Ok(PathBuf::from(
            std::str::from_utf8(bytes).context("Git path is not valid UTF-8")?,
        ))
    }
}

fn display_git_path(path: &Path) -> String {
    if let Some(text) = path.to_str()
        && !text
            .chars()
            .any(|ch| ch.is_control() || ch == '\\' || ch == '"' || ch == '\u{fffd}')
    {
        return text.to_string();
    }
    let quoted = format!("{:?}", path.as_os_str());
    quoted
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
        .unwrap_or(&quoted)
        .replace('\u{fffd}', "\\u{fffd}")
}

fn format_git_command(prefix: &str, args: &[OsString]) -> String {
    if args.is_empty() {
        prefix.to_string()
    } else {
        let rendered = args
            .iter()
            .map(|arg| shell_quote(arg.as_os_str()))
            .collect::<Vec<_>>()
            .join(" ");
        format!("{prefix} {rendered}")
    }
}

fn shell_quote(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[derive(Debug)]
struct ParsedHeader {
    old_mode: String,
    new_mode: String,
    old_oid: String,
    new_oid: String,
    status: DiffStatus,
    is_rewrite: bool,
}

#[derive(Debug, Clone)]
struct RawRecord {
    old_mode: String,
    new_mode: String,
    old_oid: String,
    new_oid: String,
    status: DiffStatus,
    is_rewrite: bool,
    old_path: PathBuf,
    new_path: Option<PathBuf>,
}

impl RawRecord {
    fn display_path(&self) -> &Path {
        self.new_path.as_deref().unwrap_or(&self.old_path)
    }

    fn rename_from(&self) -> Option<String> {
        matches!(self.status, DiffStatus::Renamed | DiffStatus::Copied)
            .then(|| display_git_path(&self.old_path))
    }
}

struct LoadResources {
    engine: difftastic::JsonDiff,
    replacements: HashMap<gix::ObjectId, gix::ObjectId>,
}

fn resolve_worktree_path(request: &DiffRequest, path: &Path) -> PathBuf {
    match &request.relative_root {
        Some(relative_root) => request.worktree_root.join(relative_root).join(path),
        None => request.worktree_root.join(path),
    }
}

fn worktree_entry_metadata(request: &DiffRequest, path: &Path) -> Option<fs::Metadata> {
    let relative = path.strip_prefix(&request.worktree_root).ok()?;
    let mut candidate = request.worktree_root.clone();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        match component {
            std::path::Component::Normal(name) => candidate.push(name),
            std::path::Component::CurDir => continue,
            _ => return None,
        }
        let metadata = candidate.symlink_metadata().ok()?;
        if components.peek().is_none() {
            return Some(metadata);
        }
        // Git 不递归进入符号链接目录；末级符号链接仍按链接内容读取。
        if !metadata.is_dir() {
            return None;
        }
    }
    None
}

fn worktree_file_exists(request: &DiffRequest, path: &Path) -> bool {
    worktree_entry_metadata(request, path)
        .is_some_and(|metadata| metadata.is_file() || metadata.file_type().is_symlink())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn load_limits_skip_oversized_old_and_new_contents() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path();
        init_repo(repo);
        fs::write(repo.join("old.txt"), "x".repeat(65)).unwrap();
        fs::write(repo.join("new.txt"), "before\n").unwrap();
        fs::write(repo.join("small.txt"), "before\n").unwrap();
        commit_all(repo, "init");
        run(repo, ["git", "gc", "--prune=now"]);
        fs::write(repo.join("old.txt"), "after\n").unwrap();
        fs::write(repo.join("new.txt"), "x".repeat(65)).unwrap();
        fs::write(repo.join("small.txt"), "after\n").unwrap();
        let files = load_diffs_with_limits(
            &request(repo, vec!["HEAD".into()], true, DiffMode::Revisions),
            DiffLimits {
                max_file_bytes: 64,
                max_total_bytes: 1_000,
                max_files: 10,
            },
        )
        .unwrap();
        assert_eq!(files.len(), 3);
        for path in ["old.txt", "new.txt"] {
            let file = files.iter().find(|file| file.path == path).unwrap();
            assert!(file.hunks.is_empty());
            assert!(file_text(file).contains("per-side limit (64 bytes)"));
        }
        let small = files.iter().find(|file| file.path == "small.txt").unwrap();
        assert!(file_text(small).contains("before"));
        assert!(file_text(small).contains("after"));
    }

    #[test]
    fn load_limits_count_both_sides_and_keep_later_files_that_fit() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path();
        init_repo(repo);
        for (path, size) in [("a.txt", 8), ("b.txt", 12), ("c.txt", 2)] {
            fs::write(repo.join(path), "a".repeat(size)).unwrap();
        }
        commit_all(repo, "init");
        for (path, size) in [("a.txt", 8), ("b.txt", 12), ("c.txt", 2)] {
            fs::write(repo.join(path), "b".repeat(size)).unwrap();
        }
        let request = request(repo, vec!["HEAD".into()], true, DiffMode::Revisions);
        let files = load_diffs_with_limits(
            &request,
            DiffLimits {
                max_file_bytes: 12,
                max_total_bytes: 20,
                max_files: 10,
            },
        )
        .unwrap();
        assert_eq!(
            files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["a.txt", "b.txt", "c.txt"]
        );
        assert!(!files[0].hunks.is_empty());
        assert!(files[1].hunks.is_empty());
        assert!(file_text(&files[1]).contains("total input limit (20 bytes)"));
        assert!(!files[2].hunks.is_empty());

        let files = load_diffs_with_limits(
            &request,
            DiffLimits {
                max_file_bytes: 12,
                max_total_bytes: 100,
                max_files: 1,
            },
        )
        .unwrap();
        assert_eq!(files.len(), 3);
        assert!(!files[0].hunks.is_empty());
        for file in &files[1..] {
            assert!(file.hunks.is_empty());
            assert!(file_text(file).contains("file count limit (1) reached"));
        }
    }

    #[test]
    fn load_limits_skip_large_untracked_file_without_loading_contents() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path();
        init_repo(repo);
        fs::write(repo.join("base.txt"), "base\n").unwrap();
        commit_all(repo, "init");
        // 仅设置长度，不生成大段测试数据。
        fs::File::create(repo.join("large.bin"))
            .unwrap()
            .set_len(16 * 1024 * 1024)
            .unwrap();
        let files = load_diffs_with_limits(
            &request(repo, vec![], true, DiffMode::WorktreeAggregate),
            DiffLimits {
                max_file_bytes: 10 * 1024 * 1024,
                max_total_bytes: 128 * 1024 * 1024,
                max_files: 5_000,
            },
        )
        .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "large.bin");
        assert_eq!(files[0].status, DiffStatus::Created);
        assert!(files[0].hunks.is_empty());
        assert!(file_text(&files[0]).contains("file size (16777216 bytes)"));
    }

    #[test]
    fn load_limits_apply_to_conflict_worktree_contents() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path();
        init_repo(repo);
        fs::write(repo.join("conflict.txt"), "base\n").unwrap();
        commit_all(repo, "init");
        run(repo, ["git", "checkout", "-q", "-b", "other"]);
        fs::write(repo.join("conflict.txt"), "other\n").unwrap();
        commit_all(repo, "other");
        run(repo, ["git", "checkout", "-q", "-"]);
        fs::write(repo.join("conflict.txt"), "main\n").unwrap();
        commit_all(repo, "main");
        let merge = Command::new("git")
            .args(["merge", "other"])
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(!merge.status.success());
        let files = load_diffs_with_limits(
            &request(repo, vec![], true, DiffMode::Unstaged),
            DiffLimits {
                max_file_bytes: 8,
                max_total_bytes: 100,
                max_files: 10,
            },
        )
        .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, DiffStatus::Unmerged);
        assert!(files[0].hunks.is_empty());
        assert!(file_text(&files[0]).contains("per-side limit (8 bytes)"));
    }

    #[test]
    fn gix_blob_loading_preserves_packed_objects_and_replace_refs() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path();
        init_repo(repo);
        fs::write(repo.join("value.txt"), "ORIGINAL_BLOB\n").unwrap();
        commit_all(repo, "original");
        let original =
            String::from_utf8(run_output(repo, ["git", "rev-parse", "HEAD:value.txt"])).unwrap();
        fs::write(repo.join("replacement"), "REPLACED_BLOB\n").unwrap();
        let replacement = String::from_utf8(run_output(
            repo,
            ["git", "hash-object", "-w", "replacement"],
        ))
        .unwrap();
        fs::remove_file(repo.join("replacement")).unwrap();
        run(
            repo,
            ["git", "replace", original.trim(), replacement.trim()],
        );
        run(repo, ["git", "gc", "--prune=now"]);
        fs::write(repo.join("value.txt"), "CURRENT_WORKTREE\n").unwrap();
        let files = load_diffs(&request(
            repo,
            vec!["HEAD".into()],
            true,
            DiffMode::Revisions,
        ))
        .unwrap();
        let text = file_text(&files[0]);
        assert!(text.contains("REPLACED_BLOB"), "{text}");
        assert!(text.contains("CURRENT_WORKTREE"), "{text}");
        assert!(!text.contains("ORIGINAL_BLOB"), "{text}");
        run(repo, ["git", "config", "core.useReplaceRefs", "false"]);
        let files = load_diffs(&request(
            repo,
            vec!["HEAD".into()],
            true,
            DiffMode::Revisions,
        ))
        .unwrap();
        let text = file_text(&files[0]);
        assert!(text.contains("ORIGINAL_BLOB"), "{text}");
        assert!(!text.contains("REPLACED_BLOB"), "{text}");
    }

    #[test]
    fn gix_blob_loading_handles_linked_worktree_with_alternate_objects() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        init_repo(&source);
        fs::write(source.join("value.txt"), "ALTERNATE_ORIGINAL\n").unwrap();
        commit_all(&source, "original");
        run(
            temp.path(),
            ["git", "clone", "-q", "--shared", "source", "clone"],
        );
        let clone = temp.path().join("clone");
        run(
            &clone,
            [
                "git",
                "worktree",
                "add",
                "-q",
                "--detach",
                "../linked",
                "HEAD",
            ],
        );
        let linked = temp.path().join("linked");
        fs::write(linked.join("value.txt"), "LINKED_WORKTREE\n").unwrap();
        let files = load_diffs(&request(
            &linked,
            vec!["HEAD".into()],
            true,
            DiffMode::Revisions,
        ))
        .unwrap();
        let text = file_text(&files[0]);
        assert!(text.contains("ALTERNATE_ORIGINAL"), "{text}");
        assert!(text.contains("LINKED_WORKTREE"), "{text}");
    }

    #[test]
    fn gix_attributes_match_native_git_precedence_and_index_fallback() {
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path().join("repo");
        fs::create_dir(&repo_path).unwrap();
        init_repo(&repo_path);
        let global = temp.path().join("global-attributes");
        fs::write(&global, "*.txt -diff\n").unwrap();
        run(
            &repo_path,
            [
                "git",
                "config",
                "core.attributesFile",
                global.to_str().unwrap(),
            ],
        );
        fs::create_dir(repo_path.join("sub")).unwrap();
        fs::write(
            repo_path.join(".gitattributes"),
            "root.txt diff\nmacro.txt binary\n",
        )
        .unwrap();
        fs::write(repo_path.join("sub/.gitattributes"), "normal.txt diff\n").unwrap();
        commit_all(&repo_path, "attributes");
        // 缺失的工作区 attributes 应从 index 获取；info/attributes 优先级更高。
        fs::remove_file(repo_path.join("sub/.gitattributes")).unwrap();
        fs::write(repo_path.join(".git/info/attributes"), "root.txt -diff\n").unwrap();
        let paths = [
            "global.txt",
            "root.txt",
            "macro.txt",
            "sub/normal.txt",
            "spaced name.txt",
            "line\nname.txt",
        ]
        .map(str::to_owned);
        let repo = gix::discover_with_environment_overrides(&repo_path).unwrap();
        let attrs = gix_binary_attributes(
            &repo,
            &paths,
            &request(&repo_path, vec![], true, DiffMode::WorktreeAggregate),
        )
        .unwrap();
        let native = Command::new("git")
            .current_dir(&repo_path)
            .args(["check-attr", "-z", "diff", "binary", "--"])
            .args(&paths)
            .output()
            .unwrap();
        assert!(native.status.success());
        let fields = native.stdout.split(|b| *b == 0).collect::<Vec<_>>();
        for group in fields.as_chunks::<6>().0 {
            let path = std::str::from_utf8(group[0]).unwrap();
            let binary = (group[1] == b"diff" && group[2] == b"unset")
                || (group[4] == b"binary" && group[5] == b"set");
            assert_eq!(attrs[path], binary, "{path}");
        }
        assert!(attrs["root.txt"]);
        assert!(attrs["macro.txt"]);
        assert!(!attrs["sub/normal.txt"]);
    }

    #[test]
    fn parse_modified_record() {
        let bytes = b":100644 100644 78981922613b2afb6025042ff6bd878ac1994e85 0000000000000000000000000000000000000000 M\0f.txt\0";
        let records = parse_raw_records(bytes).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, DiffStatus::Modified);
        assert_eq!(records[0].old_path, Path::new("f.txt"));
        assert_eq!(records[0].new_path, None);
    }

    #[test]
    fn parse_rename_record() {
        let bytes = b":100644 100644 587be6b4c3f93f93c489c0111bba5596147a26cb b77b4eb1d946f923f61785536da9ca5af6909f06 R050\0old.txt\0new.txt\0";
        let records = parse_raw_records(bytes).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, DiffStatus::Renamed);
        assert_eq!(records[0].old_path, Path::new("old.txt"));
        assert_eq!(records[0].new_path.as_deref(), Some(Path::new("new.txt")));
    }

    #[test]
    fn parse_typechange_record() {
        let bytes = b":100644 120000 b9bca015cb931d1a3cdb0c9f8511a70f2f2e7023 1de565933b05f74c75ff9a6520af5f9f8a5a2f1d T\0a.txt\0";
        let records = parse_raw_records(bytes).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, DiffStatus::TypeChanged);
        assert_eq!(records[0].old_path, Path::new("a.txt"));
        assert_eq!(records[0].new_path, None);
    }

    #[test]
    fn parse_status_token_rejects_unknown_status() {
        let err = parse_status_token("X").unwrap_err().to_string();
        assert!(err.contains("unsupported"));
    }

    #[test]
    fn load_diffs_shows_content_for_staged_added_file_against_head() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path();

        init_repo(repo);
        fs::write(repo.join("base.txt"), "base\n").unwrap();
        commit_all(repo, "init");

        fs::write(repo.join("new.txt"), "hello\nworld\n").unwrap();
        run(repo, ["git", "add", "new.txt"]);

        let request = request(repo, vec!["HEAD".into()], true, DiffMode::Revisions);

        let files = load_diffs(&request).unwrap();
        let added = files
            .iter()
            .find(|file| file.path == "new.txt")
            .unwrap_or_else(|| panic!("new.txt missing from files: {:?}", files));

        assert_eq!(added.status, DiffStatus::Created);
        let rendered = file_text(added);
        assert!(rendered.contains("hello"));
        assert!(rendered.contains("world"));
    }

    #[test]
    fn load_diffs_shows_deleted_file_content_against_head() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path();

        init_repo(repo);
        fs::write(repo.join("gone.txt"), "gone\nsoon\n").unwrap();
        commit_all(repo, "init");

        run(repo, ["git", "rm", "-q", "gone.txt"]);

        let files = load_diffs(&request(
            repo,
            vec!["HEAD".into()],
            true,
            DiffMode::Revisions,
        ))
        .unwrap();
        let deleted = files.iter().find(|file| file.path == "gone.txt").unwrap();

        assert_eq!(deleted.status, DiffStatus::Deleted);
        let rendered = file_text(deleted);
        assert!(rendered.contains("gone"));
        assert!(rendered.contains("soon"));
    }

    #[test]
    fn load_diffs_commit_to_commit_deletion_ignores_unrelated_worktree_file() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path();

        init_repo(repo);
        fs::write(repo.join("ghost.txt"), "tracked\n").unwrap();
        commit_all(repo, "c1");

        run(repo, ["git", "rm", "-q", "ghost.txt"]);
        commit_all(repo, "c2");

        fs::write(repo.join("ghost.txt"), "worktree-only\n").unwrap();

        let files = load_diffs(&request(
            repo,
            vec!["HEAD~1".into(), "HEAD".into()],
            false,
            DiffMode::Revisions,
        ))
        .unwrap();
        let deleted = files.iter().find(|file| file.path == "ghost.txt").unwrap();

        assert_eq!(deleted.status, DiffStatus::Deleted);
        let rendered = file_text(deleted);
        assert!(rendered.contains("tracked"));
        assert!(!rendered.contains("worktree-only"));
    }

    #[test]
    fn load_diffs_cached_ignores_unstaged_follow_up_changes() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path();

        init_repo(repo);
        fs::write(repo.join("a.txt"), "base\n").unwrap();
        commit_all(repo, "init");

        fs::write(repo.join("a.txt"), "staged\n").unwrap();
        run(repo, ["git", "add", "a.txt"]);
        fs::write(repo.join("a.txt"), "unstaged\n").unwrap();

        let files = load_diffs(&request(
            repo,
            vec!["--cached".into()],
            false,
            DiffMode::Cached,
        ))
        .unwrap();
        let file = files.iter().find(|file| file.path == "a.txt").unwrap();

        assert_eq!(file.status, DiffStatus::Modified);
        let rendered = file_text(file);
        assert!(rendered.contains("staged"));
        assert!(!rendered.contains("unstaged"));
    }

    #[test]
    fn load_diffs_commit_to_commit_uses_commit_content_not_worktree() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path();

        init_repo(repo);
        fs::write(repo.join("a.txt"), "one\n").unwrap();
        commit_all(repo, "c1");

        fs::write(repo.join("a.txt"), "two\n").unwrap();
        commit_all(repo, "c2");

        fs::write(repo.join("a.txt"), "worktree-only\n").unwrap();

        let files = load_diffs(&request(
            repo,
            vec!["HEAD~1".into(), "HEAD".into()],
            false,
            DiffMode::Revisions,
        ))
        .unwrap();
        let file = files.iter().find(|file| file.path == "a.txt").unwrap();

        let rendered = file_text(file);
        assert!(rendered.contains("two"));
        assert!(!rendered.contains("worktree-only"));
    }

    #[test]
    fn load_diffs_relative_uses_relative_paths() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path();

        init_repo(repo);
        fs::create_dir_all(repo.join("sub")).unwrap();
        fs::write(repo.join("sub/f.txt"), "one\n").unwrap();
        commit_all(repo, "c1");

        fs::write(repo.join("sub/f.txt"), "one\ntwo\n").unwrap();

        let mut req = request(
            repo,
            vec!["HEAD".into(), "--relative=sub".into()],
            true,
            DiffMode::Revisions,
        );
        req.relative_root = Some(PathBuf::from("sub"));
        let files = load_diffs(&req).unwrap();
        let file = files.iter().find(|file| file.path == "f.txt").unwrap();

        assert_eq!(file.status, DiffStatus::Modified);
        assert_eq!(file.path, "f.txt");
        assert!(file_text(file).contains("two"));
    }

    #[test]
    fn load_diffs_shows_rename_with_modified_content() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path();

        init_repo(repo);
        fs::write(repo.join("old.txt"), "x\n").unwrap();
        commit_all(repo, "c1");

        run(repo, ["git", "mv", "old.txt", "new.txt"]);
        fs::write(repo.join("new.txt"), "x\ny\n").unwrap();
        run(repo, ["git", "add", "new.txt"]);

        let files = load_diffs(&request(
            repo,
            vec!["HEAD".into(), "-M".into()],
            true,
            DiffMode::Revisions,
        ))
        .unwrap();
        let file = files.iter().find(|file| file.path == "new.txt").unwrap();

        assert_eq!(file.status, DiffStatus::Renamed);
        assert_eq!(file.old_path.as_deref(), Some("old.txt"));
        let rendered = file_text(file);
        assert!(rendered.contains("Renamed from old.txt to new.txt"));
        assert!(rendered.contains("y"));
    }

    #[test]
    fn split_text_lines_strips_crlf_without_dropping_trailing_blank_line() {
        let lines = split_text_lines("alpha\r\nbeta\r\n");
        assert_eq!(lines, vec!["alpha", "beta"]);
    }

    #[test]
    fn split_text_lines_keeps_real_trailing_blank_line() {
        let lines = split_text_lines("alpha\n\n");
        assert_eq!(lines, vec!["alpha", ""]);
    }

    #[test]
    fn chunk_only_hunks_preserve_blank_rhs_lines_inside_added_file() {
        let chunks = vec![vec![
            DifftJsonLine {
                lhs: None,
                rhs: Some(DifftJsonSide {
                    line_number: 0,
                    changes: vec![],
                }),
            },
            DifftJsonLine {
                lhs: None,
                rhs: Some(DifftJsonSide {
                    line_number: 2,
                    changes: vec![],
                }),
            },
        ]];

        let hunks = build_chunk_only_hunks("", "Text", &chunks, &[], &["first", "", "third"], 3);
        let rows = &hunks[0].rows;

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].rhs.as_ref().map(|side| side.line_number), Some(2));
        assert_eq!(
            rows[1].rhs.as_ref().map(|side| side.text.as_str()),
            Some("")
        );
    }

    #[test]
    fn aligned_hunks_preserve_blank_rhs_lines_inside_added_file() {
        let aligned_lines = vec![(None, Some(0)), (None, Some(2))];
        let chunks = vec![vec![
            DifftJsonLine {
                lhs: None,
                rhs: Some(DifftJsonSide {
                    line_number: 0,
                    changes: vec![],
                }),
            },
            DifftJsonLine {
                lhs: None,
                rhs: Some(DifftJsonSide {
                    line_number: 2,
                    changes: vec![],
                }),
            },
        ]];

        let hunks = build_hunks(
            "",
            "Text",
            &aligned_lines,
            chunks,
            &[],
            &["first", "", "third"],
            3,
        )
        .unwrap();
        let rows = &hunks[0].rows;

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].rhs.as_ref().map(|side| side.line_number), Some(2));
        assert_eq!(
            rows[1].rhs.as_ref().map(|side| side.text.as_str()),
            Some("")
        );
    }

    #[test]
    fn full_side_fallback_preserves_blank_lines_for_created_files() {
        let hunks = build_full_side_fallback("", "Text", DiffStatus::Created, &[], &["a", "", "b"]);
        let rows = &hunks[0].rows;
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].rhs.as_ref().map(|side| side.line_number), Some(2));
        assert_eq!(
            rows[1].rhs.as_ref().map(|side| side.text.as_str()),
            Some("")
        );
    }

    #[test]
    fn full_side_fallback_preserves_blank_lines_for_deleted_files() {
        let hunks = build_full_side_fallback("", "Text", DiffStatus::Deleted, &["a", "", "b"], &[]);
        let rows = &hunks[0].rows;
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].lhs.as_ref().map(|side| side.line_number), Some(2));
        assert_eq!(
            rows[1].lhs.as_ref().map(|side| side.text.as_str()),
            Some("")
        );
    }

    #[test]
    fn display_language_falls_back_to_highlight_detection_for_text_like_difft_labels() {
        assert_eq!(resolved_display_language("Text", "justfile"), "Just");
        assert_eq!(resolved_display_language("Text", "README.md"), "Markdown");
        assert_eq!(resolved_display_language("Rust", "src/main.rs"), "Rust");
    }

    #[test]
    fn aligned_line_index_maps_pairs_to_positions() {
        let index = aligned_line_index(&[(Some(1), Some(1)), (Some(2), Some(3)), (None, Some(4))]);
        assert_eq!(index.get(&(Some(1), Some(1))), Some(&0));
        assert_eq!(index.get(&(Some(2), Some(3))), Some(&1));
        assert_eq!(index.get(&(None, Some(4))), Some(&2));
    }

    #[test]
    fn rows_visible_lines_collects_both_sides() {
        let rows = vec![
            DiffRow::new(
                None,
                Some(DiffSide {
                    line_number: 10,
                    text: "x".to_string(),
                    diff_ranges: vec![],
                }),
            ),
            DiffRow::new(
                Some(DiffSide {
                    line_number: 30,
                    text: "ctx".to_string(),
                    diff_ranges: vec![],
                }),
                Some(DiffSide {
                    line_number: 11,
                    text: "ctx".to_string(),
                    diff_ranges: vec![],
                }),
            ),
        ];

        let visible = {
            let mut lhs = HashSet::new();
            let mut rhs = HashSet::new();

            for row in &rows {
                if let Some(side) = &row.lhs {
                    lhs.insert(side.line_number);
                }
                if let Some(side) = &row.rhs {
                    rhs.insert(side.line_number);
                }
            }

            (lhs, rhs)
        };
        assert!(visible.0.contains(&30));
        assert!(visible.1.contains(&10));
        assert!(visible.1.contains(&11));
    }

    #[test]
    fn fill_between_matches_difft_gap_expansion() {
        assert_eq!(
            fill_between(Some(1), Some(3), Some(10), Some(13)),
            vec![(Some(2), Some(11)), (None, Some(12))]
        );
    }

    #[test]
    fn pad_before_and_after_use_context_plus_one() {
        assert_eq!(pad_before(5, 3), vec![1, 2, 3, 4]);
        assert_eq!(pad_after(5, 10, 3), vec![6, 7, 8, 9]);
    }

    #[test]
    fn load_diffs_shows_copy_detection_when_requested() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path();

        init_repo(repo);
        fs::write(repo.join("old.txt"), "x\n".repeat(20)).unwrap();
        commit_all(repo, "c1");

        fs::write(repo.join("copy.txt"), format!("{}tail\n", "x\n".repeat(20))).unwrap();
        run(repo, ["git", "add", "copy.txt"]);

        let raw = run_output(
            repo,
            [
                "git",
                "diff",
                "HEAD",
                "--raw",
                "-z",
                "-C",
                "--find-copies-harder",
            ],
        );
        let records = parse_raw_records(&raw).unwrap();
        assert!(
            records
                .iter()
                .any(|record| record.status == DiffStatus::Copied)
        );

        let files = load_diffs(&request(
            repo,
            vec!["HEAD".into(), "-C".into(), "--find-copies-harder".into()],
            true,
            DiffMode::Revisions,
        ))
        .unwrap();
        let file = files.iter().find(|file| file.path == "copy.txt").unwrap();

        assert_eq!(file.status, DiffStatus::Copied);
        assert_eq!(file.old_path.as_deref(), Some("old.txt"));
        let rendered = file_text(file);
        assert!(rendered.contains("tail"));
    }

    #[test]
    fn chunk_only_hunks_match_difft_merge_shape_for_app_like_input() {
        let lhs_owned = (0..136).map(|idx| format!("lhs-{idx}")).collect::<Vec<_>>();
        let rhs_owned = (0..82).map(|idx| format!("rhs-{idx}")).collect::<Vec<_>>();
        let lhs = lhs_owned.iter().map(String::as_str).collect::<Vec<_>>();
        let rhs = rhs_owned.iter().map(String::as_str).collect::<Vec<_>>();
        let chunks = vec![
            vec![line(None, Some(17)), line(Some(18), None)],
            vec![
                line(Some(37), None),
                line(Some(38), None),
                line(Some(39), Some(36)),
                line(Some(42), Some(39)),
            ],
            vec![line(Some(48), None), line(Some(49), None)],
            vec![line(Some(58), None)],
            vec![
                line(None, Some(57)),
                line(None, Some(58)),
                line(Some(63), Some(59)),
                line(Some(64), Some(60)),
            ],
            vec![line(Some(71), None), line(Some(81), None)],
            vec![line(Some(94), None), line(Some(106), None)],
            vec![line(Some(111), Some(81))],
            vec![line(Some(119), None), line(Some(135), None)],
        ];

        let hunks = build_chunk_only_hunks("src/app.rs", "Rust", &chunks, &lhs, &rhs, 3);
        assert_eq!(hunks.len(), 4);
    }

    #[test]
    fn load_diffs_merge_base_compares_against_common_ancestor() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path();

        init_repo(repo);
        fs::write(repo.join("a.txt"), "base\n").unwrap();
        commit_all(repo, "base");

        run(repo, ["git", "switch", "-c", "feature"]);
        fs::write(repo.join("a.txt"), "feature\n").unwrap();
        commit_all(repo, "feature");

        run(repo, ["git", "switch", "master"]);
        fs::write(repo.join("a.txt"), "master\n").unwrap();
        commit_all(repo, "master");

        let files = load_diffs(&request(
            repo,
            vec!["--merge-base".into(), "master".into(), "feature".into()],
            false,
            DiffMode::Revisions,
        ))
        .unwrap();
        let file = files.iter().find(|file| file.path == "a.txt").unwrap();

        let rendered = file_text(file);
        assert!(rendered.contains("feature"));
        assert!(!rendered.contains("master"));
    }

    #[test]
    fn chunk_only_hunks_preserve_context_for_rhs_only_changes() {
        let lhs = ["a", "b", "c", "d"];
        let rhs = ["a", "b", "x", "y", "c", "d"];
        let rows = expand_chunk_rows(
            &[
                DifftJsonLine {
                    lhs: None,
                    rhs: Some(DifftJsonSide {
                        line_number: 2,
                        changes: vec![DifftJsonChange {
                            start: 0,
                            end: 1,
                            highlight: DiffHighlightKind::Normal,
                        }],
                    }),
                },
                DifftJsonLine {
                    lhs: None,
                    rhs: Some(DifftJsonSide {
                        line_number: 3,
                        changes: vec![DifftJsonChange {
                            start: 0,
                            end: 1,
                            highlight: DiffHighlightKind::Normal,
                        }],
                    }),
                },
            ],
            &lhs,
            &rhs,
            2,
            0,
        );

        assert!(rows.iter().any(|row| {
            row.lhs.as_ref().map(|side| side.text.as_str()) == Some("a")
                && row.rhs.as_ref().map(|side| side.text.as_str()) == Some("a")
        }));
        assert!(rows.iter().any(|row| {
            row.rhs.as_ref().map(|side| side.text.as_str()) == Some("x") && row.lhs.is_none()
        }));
        assert!(rows.iter().any(|row| {
            row.lhs.as_ref().map(|side| side.text.as_str()) == Some("c")
                && row.rhs.as_ref().map(|side| side.text.as_str()) == Some("c")
        }));
    }

    #[test]
    fn chunk_only_hunks_merge_when_context_overlaps() {
        let lhs = ["a", "b", "c", "d", "e", "f", "g"];
        let rhs = ["a", "b", "x", "c", "d", "y", "e", "f", "g"];
        let hunks = build_chunk_only_hunks(
            "input.rs",
            "Rust",
            &[
                vec![DifftJsonLine {
                    lhs: None,
                    rhs: Some(DifftJsonSide {
                        line_number: 2,
                        changes: vec![DifftJsonChange {
                            start: 0,
                            end: 1,
                            highlight: DiffHighlightKind::Normal,
                        }],
                    }),
                }],
                vec![DifftJsonLine {
                    lhs: None,
                    rhs: Some(DifftJsonSide {
                        line_number: 5,
                        changes: vec![DifftJsonChange {
                            start: 0,
                            end: 1,
                            highlight: DiffHighlightKind::Normal,
                        }],
                    }),
                }],
            ],
            &lhs,
            &rhs,
            2,
        );

        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].title.as_deref(), Some("1/1 --- Rust"));
    }

    #[cfg(unix)]
    #[test]
    fn load_diffs_supports_typechange_records() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let repo = temp.path();

        init_repo(repo);
        fs::write(repo.join("a.txt"), "plain\n").unwrap();
        commit_all(repo, "init");

        fs::remove_file(repo.join("a.txt")).unwrap();
        fs::write(repo.join("target.txt"), "symlink-target-file-contents\n").unwrap();
        symlink("target.txt", repo.join("a.txt")).unwrap();
        run(repo, ["git", "add", "-A"]);

        let files = load_diffs(&request(
            repo,
            vec!["HEAD".into()],
            true,
            DiffMode::Revisions,
        ))
        .unwrap();
        let file = files.iter().find(|file| file.path == "a.txt").unwrap();

        assert_eq!(file.status, DiffStatus::TypeChanged);
        let rendered = file_text(file);
        assert!(rendered.contains("File type changed"));
        assert!(rendered.contains("plain"));
        assert!(rendered.contains("target.txt"));
        assert!(!rendered.contains("symlink-target-file-contents"));
    }

    #[test]
    fn load_diffs_handles_mixed_change_kinds_in_single_run() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path();

        init_repo(repo);
        fs::write(repo.join("mod.txt"), "old\n").unwrap();
        fs::write(repo.join("del.txt"), "delete\n").unwrap();
        fs::write(repo.join("rename.txt"), "rename\n").unwrap();
        commit_all(repo, "init");

        fs::write(repo.join("mod.txt"), "new\n").unwrap();
        run(repo, ["git", "rm", "-q", "del.txt"]);
        run(repo, ["git", "mv", "rename.txt", "renamed.txt"]);
        fs::write(repo.join("renamed.txt"), "rename\nplus\n").unwrap();
        run(repo, ["git", "add", "."]);
        fs::write(repo.join("add.txt"), "added\n").unwrap();
        run(repo, ["git", "add", "add.txt"]);

        let files = load_diffs(&request(
            repo,
            vec!["HEAD".into(), "-M".into()],
            true,
            DiffMode::Revisions,
        ))
        .unwrap();

        assert!(
            files
                .iter()
                .any(|file| file.path == "mod.txt" && file.status == DiffStatus::Modified)
        );
        assert!(
            files
                .iter()
                .any(|file| file.path == "del.txt" && file.status == DiffStatus::Deleted)
        );
        assert!(
            files
                .iter()
                .any(|file| file.path == "add.txt" && file.status == DiffStatus::Created)
        );
        assert!(
            files
                .iter()
                .any(|file| file.path == "renamed.txt" && file.status == DiffStatus::Renamed)
        );
    }

    #[test]
    fn load_diffs_worktree_aggregate_includes_untracked_files() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path();

        init_repo(repo);
        fs::write(repo.join("tracked.txt"), "base\n").unwrap();
        commit_all(repo, "init");

        fs::write(repo.join("untracked.txt"), "hello\nworld\n").unwrap();

        let files = load_diffs(&request(repo, vec![], true, DiffMode::WorktreeAggregate)).unwrap();
        let added = files
            .iter()
            .find(|file| file.path == "untracked.txt")
            .unwrap();

        assert_eq!(added.status, DiffStatus::Created);
        let rendered = file_text(added);
        assert!(rendered.contains("hello"));
        assert!(rendered.contains("world"));
    }

    #[test]
    fn load_diffs_worktree_aggregate_prefers_worktree_over_staged_snapshot() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path();

        init_repo(repo);
        fs::write(repo.join("a.txt"), "base\n").unwrap();
        commit_all(repo, "init");

        fs::write(repo.join("a.txt"), "staged\n").unwrap();
        run(repo, ["git", "add", "a.txt"]);
        fs::write(repo.join("a.txt"), "worktree\n").unwrap();

        let files = load_diffs(&request(repo, vec![], true, DiffMode::WorktreeAggregate)).unwrap();
        let file = files.iter().find(|file| file.path == "a.txt").unwrap();

        assert_eq!(file.status, DiffStatus::Modified);
        let rendered = file_text(file);
        assert!(rendered.contains("worktree"));
        assert!(!rendered.contains("staged"));
    }

    #[test]
    fn load_diffs_worktree_aggregate_supports_unborn_head() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path();

        init_repo(repo);
        fs::write(repo.join("new.txt"), "first\n").unwrap();

        let files = load_diffs(&request(repo, vec![], true, DiffMode::WorktreeAggregate)).unwrap();
        let file = files.iter().find(|file| file.path == "new.txt").unwrap();

        assert_eq!(file.status, DiffStatus::Created);
        assert!(file_text(file).contains("first"));
    }

    fn init_repo(repo: &Path) {
        run(repo, ["git", "init", "-q"]);
        run(repo, ["git", "config", "user.name", "Test User"]);
        run(repo, ["git", "config", "user.email", "test@example.com"]);
    }

    fn commit_all(repo: &Path, message: &str) {
        run(repo, ["git", "add", "."]);
        run(repo, ["git", "commit", "-qm", message]);
    }

    fn request(
        repo: &Path,
        git_args: Vec<OsString>,
        uses_worktree: bool,
        mode: DiffMode,
    ) -> DiffRequest {
        DiffRequest {
            revisions: git_args
                .iter()
                .take_while(|arg| arg.as_os_str() != OsStr::new("--"))
                .filter(|arg| !arg.as_encoded_bytes().starts_with(b"-"))
                .cloned()
                .collect(),
            git_args,
            difft_context: None,
            worktree_root: repo.to_path_buf(),
            invocation_dir: None,
            cancellation: Default::default(),
            progress: Default::default(),
            uses_worktree,
            relative_root: None,
            pathspecs: Vec::new(),
            mode,
        }
    }

    fn run<const N: usize>(cwd: &Path, args: [&str; N]) {
        let status = Command::new(args[0])
            .args(&args[1..])
            .current_dir(cwd)
            .status()
            .unwrap();
        assert!(status.success(), "command failed: {}", args.join(" "));
    }

    fn run_output<const N: usize>(cwd: &Path, args: [&str; N]) -> Vec<u8> {
        let output = Command::new(args[0])
            .args(&args[1..])
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "command failed: {}",
            args.join(" ")
        );
        output.stdout
    }

    fn file_text(file: &FileDiff) -> String {
        let mut parts = file.metadata_lines.clone();
        for hunk in &file.hunks {
            for row in &hunk.rows {
                if let Some(lhs) = &row.lhs {
                    parts.push(lhs.text.clone());
                }
                if let Some(rhs) = &row.rhs {
                    parts.push(rhs.text.clone());
                }
            }
        }
        parts.join("\n")
    }

    fn line(lhs: Option<u32>, rhs: Option<u32>) -> DifftJsonLine {
        DifftJsonLine {
            lhs: lhs.map(|line_number| DifftJsonSide {
                line_number,
                changes: vec![],
            }),
            rhs: rhs.map(|line_number| DifftJsonSide {
                line_number,
                changes: vec![],
            }),
        }
    }
}
