use std::{
    cmp::Ordering,
    collections::HashSet,
    ffi::OsStr,
    path::{Path, PathBuf},
};

use clap::CommandFactory;
use clap_complete::{
    CompleteEnv,
    engine::{ArgValueCompleter, CompletionCandidate, PathCompleter, ValueCompleter},
};
use gix::revision::walk::Sorting;

use crate::cli::Cli;

const PSEUDO_REFS: &[&str] = &["ORIG_HEAD", "MERGE_HEAD", "FETCH_HEAD"];
const MIN_COMMIT_PREFIX_LEN: usize = 4;
const DEFAULT_COMMIT_ABBREV_LEN: usize = 12;
const MAX_COMMIT_CANDIDATES: usize = 8;

pub fn maybe_complete() {
    CompleteEnv::with_factory(command).complete();
}

pub fn command() -> clap::Command {
    Cli::command()
}

pub fn revision_completer() -> ArgValueCompleter {
    ArgValueCompleter::new(complete_git_revisions)
}

pub fn directory_completer() -> ArgValueCompleter {
    ArgValueCompleter::new(complete_repo_directories)
}

pub fn path_completer() -> ArgValueCompleter {
    ArgValueCompleter::new(complete_repo_files_and_directories)
}

fn complete_git_revisions(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    let Some(context) = completion_context() else {
        return Vec::new();
    };

    let mut seen = HashSet::new();
    let mut candidates = Vec::new();

    push_candidate(
        &mut candidates,
        &mut seen,
        RevisionCandidate::new("HEAD", RevisionKind::Head),
        prefix,
    );
    push_candidate(
        &mut candidates,
        &mut seen,
        RevisionCandidate::new("@", RevisionKind::At),
        prefix,
    );

    for &pseudo_ref in PSEUDO_REFS {
        if context
            .repo
            .try_find_reference(pseudo_ref)
            .ok()
            .flatten()
            .is_some()
        {
            push_candidate(
                &mut candidates,
                &mut seen,
                RevisionCandidate::new((*pseudo_ref).to_string(), RevisionKind::PseudoRef),
                prefix,
            );
        }
    }

    let current_branch = current_branch_name(&context.repo);
    if let Some(current_branch) = current_branch.as_ref() {
        push_candidate(
            &mut candidates,
            &mut seen,
            RevisionCandidate::new(current_branch.clone(), RevisionKind::CurrentBranch),
            prefix,
        );
    }

    for candidate in git_reference_candidates(&context.repo, current_branch.as_deref()) {
        push_candidate(&mut candidates, &mut seen, candidate, prefix);
    }

    for candidate in commit_prefix_candidates(&context.repo, prefix) {
        push_candidate(&mut candidates, &mut seen, candidate, prefix);
    }

    candidates.sort_by(|a, b| {
        a.sort_key
            .cmp(&b.sort_key)
            .then_with(|| a.value.cmp(&b.value))
    });
    candidates
        .into_iter()
        .map(|candidate| CompletionCandidate::new(candidate.value))
        .collect()
}

fn complete_repo_paths(current: &OsStr, directories_only: bool) -> Vec<CompletionCandidate> {
    let current_dir = completion_context()
        .and_then(|context| {
            if directories_only {
                context.workdir
            } else {
                context.completion_dir()
            }
        })
        .or_else(|| std::env::current_dir().ok());

    let completer = match (directories_only, current_dir) {
        (true, Some(current_dir)) => PathCompleter::dir().current_dir(current_dir),
        (true, None) => PathCompleter::dir(),
        (false, Some(current_dir)) => PathCompleter::any().current_dir(current_dir),
        (false, None) => PathCompleter::any(),
    };

    completer.complete(current)
}

fn complete_repo_directories(current: &OsStr) -> Vec<CompletionCandidate> {
    complete_repo_paths(current, true)
}

fn complete_repo_files_and_directories(current: &OsStr) -> Vec<CompletionCandidate> {
    complete_repo_paths(current, false)
}

fn completion_context() -> Option<CompletionContext> {
    let repo = gix::discover(".").ok()?;
    let workdir = repo.workdir().map(Path::to_path_buf);
    let prefix = repo
        .prefix()
        .ok()
        .flatten()
        .filter(|prefix| !prefix.as_os_str().is_empty())
        .map(Path::to_path_buf);
    Some(CompletionContext {
        repo,
        workdir,
        prefix,
    })
}

fn current_branch_name(repo: &gix::Repository) -> Option<String> {
    repo.head_name()
        .ok()
        .flatten()
        .map(|name| name.shorten().to_string())
}

fn git_reference_candidates(
    repo: &gix::Repository,
    current_branch: Option<&str>,
) -> Vec<RevisionCandidate> {
    let Ok(platform) = repo.references() else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    for (kind, iter) in [
        (RevisionKind::LocalBranch, platform.local_branches().ok()),
        (RevisionKind::RemoteBranch, platform.remote_branches().ok()),
        (RevisionKind::Tag, platform.tags().ok()),
    ] {
        for reference in iter.into_iter().flatten().flatten() {
            let short = reference.name().shorten().to_string();
            if current_branch.is_some_and(|branch| branch == short) {
                continue;
            }
            candidates.push(RevisionCandidate::new(short, kind));
        }
    }
    candidates
}

fn commit_prefix_candidates(repo: &gix::Repository, prefix: &str) -> Vec<RevisionCandidate> {
    if !looks_like_commit_prefix(prefix) {
        return Vec::new();
    }

    let mut walk = match repo
        .head_id()
        .ok()
        .map(|head| {
            repo.rev_walk([head.detach()])
                .sorting(Sorting::ByCommitTime(
                    gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
                ))
        })
        .and_then(|walk| walk.all().ok())
    {
        Some(walk) => walk,
        None => return Vec::new(),
    };

    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    while let Some(Ok(info)) = walk.next() {
        let hex = info.id.to_string();
        if !hex.starts_with(prefix) {
            continue;
        }

        let abbrev_len = prefix.len().max(DEFAULT_COMMIT_ABBREV_LEN).min(hex.len());
        let abbrev = hex[..abbrev_len].to_string();
        if seen.insert(abbrev.clone()) {
            candidates.push(RevisionCandidate::new(abbrev, RevisionKind::Commit));
        }
        if candidates.len() >= MAX_COMMIT_CANDIDATES {
            break;
        }
    }
    candidates
}

fn push_candidate(
    candidates: &mut Vec<ScoredRevisionCandidate>,
    seen: &mut HashSet<String>,
    candidate: RevisionCandidate,
    prefix: &str,
) {
    let Some(match_rank) = candidate.match_rank(prefix) else {
        return;
    };
    if !seen.insert(candidate.value.clone()) {
        return;
    }

    candidates.push(ScoredRevisionCandidate {
        sort_key: (
            candidate.kind.sort_rank(),
            match_rank,
            candidate.value.clone(),
        ),
        value: candidate.value,
    });
}

fn looks_like_commit_prefix(prefix: &str) -> bool {
    prefix.len() >= MIN_COMMIT_PREFIX_LEN && prefix.as_bytes().iter().all(u8::is_ascii_hexdigit)
}

#[derive(Debug)]
struct CompletionContext {
    repo: gix::Repository,
    workdir: Option<PathBuf>,
    prefix: Option<PathBuf>,
}

impl CompletionContext {
    fn completion_dir(&self) -> Option<PathBuf> {
        match (&self.workdir, &self.prefix) {
            (Some(workdir), Some(prefix)) => Some(workdir.join(prefix)),
            (Some(workdir), None) => Some(workdir.clone()),
            (None, _) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RevisionKind {
    Head,
    At,
    CurrentBranch,
    PseudoRef,
    LocalBranch,
    RemoteBranch,
    Tag,
    Commit,
}

impl RevisionKind {
    fn sort_rank(self) -> usize {
        match self {
            RevisionKind::Head => 0,
            RevisionKind::At => 1,
            RevisionKind::CurrentBranch => 2,
            RevisionKind::PseudoRef => 3,
            RevisionKind::LocalBranch => 4,
            RevisionKind::RemoteBranch => 5,
            RevisionKind::Tag => 6,
            RevisionKind::Commit => 7,
        }
    }
}

#[derive(Debug)]
struct RevisionCandidate {
    value: String,
    kind: RevisionKind,
}

impl RevisionCandidate {
    fn new(value: impl Into<String>, kind: RevisionKind) -> Self {
        Self {
            value: value.into(),
            kind,
        }
    }

    fn match_rank(&self, prefix: &str) -> Option<usize> {
        if prefix.is_empty() {
            return Some(0);
        }

        let value = self.value.as_str();
        if value.starts_with(prefix) {
            return Some(0);
        }

        let value_lower = value.to_ascii_lowercase();
        let prefix_lower = prefix.to_ascii_lowercase();
        if value_lower.starts_with(&prefix_lower) {
            return Some(1);
        }

        if value
            .split('/')
            .skip(1)
            .any(|segment| segment.starts_with(prefix))
        {
            return Some(2);
        }

        if value_lower
            .split('/')
            .skip(1)
            .any(|segment| segment.starts_with(&prefix_lower))
        {
            return Some(3);
        }

        None
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ScoredRevisionCandidate {
    sort_key: (usize, usize, String),
    value: String,
}

impl Ord for ScoredRevisionCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sort_key.cmp(&other.sort_key)
    }
}

impl PartialOrd for ScoredRevisionCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command as ProcessCommand};
    use tempfile::TempDir;

    #[test]
    fn git_revision_completer_includes_refs_and_head_aliases() {
        let repo = init_repo();
        run_git(
            repo.path(),
            ["checkout", "-b", "feature/dynamic-completion-test"],
        );
        fs::write(repo.path().join("tracked.txt"), "hello\n").unwrap();
        run_git(repo.path(), ["add", "tracked.txt"]);
        run_git(repo.path(), ["commit", "-m", "init"]);
        run_git(repo.path(), ["tag", "v0.1.0"]);

        let values = with_current_dir(repo.path(), || revision_values("f"));
        assert!(values.contains(&"feature/dynamic-completion-test".to_string()));
    }

    #[test]
    fn git_revision_completer_orders_head_current_branch_and_refs() {
        let repo = seeded_repo();
        run_git(repo.path(), ["checkout", "-b", "feature/topic"]);
        commit_file(repo.path(), "topic.txt", "topic\n", "topic");
        run_git(repo.path(), ["branch", "alpha/local"]);
        run_git(repo.path(), ["tag", "v1.0.0"]);
        run_git(
            repo.path(),
            ["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        run_git(repo.path(), ["update-ref", "ORIG_HEAD", "HEAD"]);

        let values = with_current_dir(repo.path(), || revision_values(""));

        assert_order(&values, "HEAD", "@");
        assert_order(&values, "@", "feature/topic");
        assert_order(&values, "feature/topic", "ORIG_HEAD");
        assert_order(&values, "ORIG_HEAD", "alpha/local");
        assert_order(&values, "alpha/local", "origin/main");
        assert_order(&values, "origin/main", "v1.0.0");
    }

    #[test]
    fn git_revision_completer_matches_case_insensitive_and_path_segments() {
        let repo = seeded_repo();
        run_git(repo.path(), ["checkout", "-b", "feature/topic"]);
        commit_file(repo.path(), "topic.txt", "topic\n", "topic");
        run_git(repo.path(), ["update-ref", "ORIG_HEAD", "HEAD"]);

        let pseudo = with_current_dir(repo.path(), || revision_values("or"));
        assert_eq!(pseudo, vec!["ORIG_HEAD".to_string()]);

        let segment = with_current_dir(repo.path(), || revision_values("top"));
        assert!(segment.contains(&"feature/topic".to_string()));
    }

    #[test]
    fn git_revision_completer_includes_commit_prefix_matches() {
        let repo = seeded_repo();
        commit_file(repo.path(), "second.txt", "second\n", "second");

        let full = with_current_dir(repo.path(), || {
            run_git_output(repo.path(), ["rev-parse", "HEAD"])
        });
        let full = full.trim();
        let prefix = &full[..7];

        let values = with_current_dir(repo.path(), || revision_values(prefix));
        assert!(values.iter().any(|value| value.starts_with(prefix)));
        assert!(values.iter().all(|value| value != full));
    }

    #[test]
    fn git_revision_completer_is_empty_outside_git_repo() {
        let dir = TempDir::new().unwrap();
        let values = with_current_dir(dir.path(), || revision_values("HE"));
        assert!(values.is_empty());
    }

    #[test]
    fn path_completion_uses_repo_prefix_as_base_dir() {
        let repo = seeded_repo();
        fs::create_dir_all(repo.path().join("src/module")).unwrap();
        fs::write(repo.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(repo.path().join("src/module/lib.rs"), "pub fn f() {}\n").unwrap();

        let values = with_current_dir(&repo.path().join("src"), || path_values("ma"));
        assert!(values.contains(&"main.rs".to_string()));
        assert!(!values.iter().any(|value| value.contains("src/main.rs")));
    }

    #[test]
    fn relative_completion_uses_repository_root() {
        let repo = seeded_repo();
        fs::create_dir_all(repo.path().join("src/module")).unwrap();

        let values = with_current_dir(&repo.path().join("src"), || directory_values("src/mo"));
        let separator = std::path::MAIN_SEPARATOR;
        let expected = format!("src{separator}module{separator}");
        assert!(values.contains(&expected), "{values:?}");
    }

    fn revision_values(prefix: &str) -> Vec<String> {
        complete_git_revisions(OsStr::new(prefix))
            .into_iter()
            .map(candidate_value)
            .collect()
    }

    fn path_values(prefix: &str) -> Vec<String> {
        complete_repo_paths(OsStr::new(prefix), false)
            .into_iter()
            .map(candidate_value)
            .collect()
    }

    fn directory_values(prefix: &str) -> Vec<String> {
        complete_repo_paths(OsStr::new(prefix), true)
            .into_iter()
            .map(candidate_value)
            .collect()
    }

    fn candidate_value(candidate: CompletionCandidate) -> String {
        candidate.get_value().to_string_lossy().into_owned()
    }

    fn assert_order(values: &[String], left: &str, right: &str) {
        let left_index = values.iter().position(|value| value == left).unwrap();
        let right_index = values.iter().position(|value| value == right).unwrap();
        assert!(
            left_index < right_index,
            "{left} should appear before {right}: {values:?}"
        );
    }

    fn seeded_repo() -> TempDir {
        let repo = init_repo();
        commit_file(repo.path(), "tracked.txt", "hello\n", "init");
        repo
    }

    fn commit_file(repo: &Path, path: &str, content: &str, message: &str) {
        fs::write(repo.join(path), content).unwrap();
        run_git(repo, ["add", path]);
        run_git(repo, ["commit", "-m", message]);
    }

    fn init_repo() -> TempDir {
        let repo = TempDir::new().unwrap();
        run_git(repo.path(), ["init"]);
        run_git(repo.path(), ["config", "user.name", "Test User"]);
        run_git(repo.path(), ["config", "user.email", "test@example.com"]);
        repo
    }

    fn with_current_dir<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
        let previous_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();
        let result = f();
        std::env::set_current_dir(previous_dir).unwrap();
        result
    }

    fn run_git<I, S>(cwd: &Path, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let status = ProcessCommand::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn run_git_output<I, S>(cwd: &Path, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = ProcessCommand::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    }
}
