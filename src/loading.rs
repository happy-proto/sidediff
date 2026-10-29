//! 加载进度和诊断共用同一条计算路径；并行阶段的累计耗时不能相加为墙钟时间。
use serde::Serialize;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

#[derive(Default, Debug, Clone, Serialize)]
pub struct Timing {
    pub calls: usize,
    pub total_ms: f64,
    pub max_ms: f64,
}
#[derive(Debug, Clone, Serialize)]
pub struct FileTiming {
    pub path: String,
    pub elapsed_ms: f64,
    pub bytes: usize,
    pub skipped: bool,
}
#[derive(Debug)]
struct Active {
    path: String,
    started: Instant,
    phase: &'static str,
    phase_started: Instant,
    bytes: usize,
}
#[derive(Debug)]
struct State {
    stage: &'static str,
    stage_started: Instant,
    total: usize,
    workers: usize,
    completed: usize,
    skipped: usize,
    active: BTreeMap<usize, Active>,
    files: Vec<FileTiming>,
    phases: BTreeMap<String, Timing>,
    stages: BTreeMap<String, f64>,
}
#[derive(Debug)]
pub struct Progress {
    started: Instant,
    profile: AtomicBool,
    state: Mutex<State>,
}
impl Default for Progress {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            profile: AtomicBool::new(false),
            state: Mutex::new(State {
                stage: "Resolving revisions",
                stage_started: now,
                total: 0,
                workers: 0,
                completed: 0,
                skipped: 0,
                active: BTreeMap::new(),
                files: Vec::new(),
                phases: BTreeMap::new(),
                stages: BTreeMap::new(),
            }),
        }
    }
}
#[derive(Serialize)]
pub struct Report {
    pub elapsed_ms: f64,
    pub stage: String,
    pub total_files: usize,
    pub worker_threads: usize,
    pub completed_files: usize,
    pub skipped_files: usize,
    pub stage_ms: BTreeMap<String, f64>,
    pub phase_timings: BTreeMap<String, Timing>,
    pub files: Vec<FileTiming>,
    pub active: Vec<ActiveTiming>,
}
#[derive(Serialize)]
pub struct ActiveTiming {
    pub path: String,
    pub phase: String,
    pub elapsed_ms: f64,
    pub phase_ms: f64,
}
impl Progress {
    pub fn enable_profile(&self) {
        self.profile.store(true, Ordering::Relaxed);
    }
    pub fn stage(&self, stage: &'static str) {
        let mut s = self.state.lock().unwrap();
        let old = s.stage.to_owned();
        let elapsed = s.stage_started.elapsed().as_secs_f64() * 1000.;
        *s.stages.entry(old).or_default() += elapsed;
        s.stage = stage;
        s.stage_started = Instant::now();
    }
    pub fn plan(&self, total: usize) {
        self.state.lock().unwrap().total = total;
    }
    pub fn workers(&self, workers: usize) {
        self.state.lock().unwrap().workers = workers;
    }
    pub fn start_file(&self, index: usize, path: String) {
        let now = Instant::now();
        self.state.lock().unwrap().active.insert(
            index,
            Active {
                path,
                started: now,
                phase: "Reading content",
                phase_started: now,
                bytes: 0,
            },
        );
    }
    pub fn finish_file(&self, index: usize, skipped: bool) {
        let mut s = self.state.lock().unwrap();
        if let Some(active) = s.active.remove(&index) {
            s.completed += 1;
            s.skipped += usize::from(skipped);
            if self.profile.load(Ordering::Relaxed) {
                s.files.push(FileTiming {
                    path: active.path,
                    elapsed_ms: active.started.elapsed().as_secs_f64() * 1000.,
                    bytes: active.bytes,
                    skipped,
                });
            }
        }
    }
    pub fn report(&self) -> Report {
        let s = self.state.lock().unwrap();
        let mut stage_ms = s.stages.clone();
        if s.stage != "Ready" {
            *stage_ms.entry(s.stage.to_owned()).or_default() +=
                s.stage_started.elapsed().as_secs_f64() * 1000.;
        }
        let mut files = s.files.clone();
        files.sort_by(|a, b| b.elapsed_ms.total_cmp(&a.elapsed_ms));
        Report {
            elapsed_ms: if s.stage == "Ready" {
                s.stage_started.duration_since(self.started)
            } else {
                self.started.elapsed()
            }
            .as_secs_f64()
                * 1000.,
            stage: s.stage.to_owned(),
            total_files: s.total,
            worker_threads: s.workers,
            completed_files: s.completed,
            skipped_files: s.skipped,
            stage_ms,
            phase_timings: s.phases.clone(),
            files,
            active: s
                .active
                .values()
                .map(|a| ActiveTiming {
                    path: a.path.clone(),
                    phase: a.phase.to_owned(),
                    elapsed_ms: a.started.elapsed().as_secs_f64() * 1000.,
                    phase_ms: a.phase_started.elapsed().as_secs_f64() * 1000.,
                })
                .collect(),
        }
    }
    pub fn display(&self) -> String {
        let r = self.report();
        let spinner = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏']
            [(self.started.elapsed().as_millis() / 100 % 10) as usize];
        let mut text = format!(
            "{spinner} Loading differences…  Elapsed: {:.1}s\n{}  {}/{} files  ({} omitted)\nq / Esc / Ctrl-c to cancel\n",
            r.elapsed_ms / 1000.,
            r.stage,
            r.completed_files,
            r.total_files,
            r.skipped_files
        );
        for file in r.active.iter().take(8) {
            text.push_str(&format!(
                "\n{}  — {} {:.1}s (file {:.1}s)",
                file.path.escape_debug(),
                phase_label(&file.phase),
                file.phase_ms / 1000.,
                file.elapsed_ms / 1000.
            ));
        }
        text
    }
}

thread_local! { static CURRENT: RefCell<Option<(Arc<Progress>, Option<usize>)>> = const { RefCell::new(None) }; }
pub fn context<T>(progress: &Arc<Progress>, file_id: Option<usize>, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<(Arc<Progress>, Option<usize>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }
    let _restore = Restore(CURRENT.with(|slot| slot.replace(Some((progress.clone(), file_id)))));
    f()
}
pub fn input_bytes(bytes: usize) {
    CURRENT.with(|slot| {
        if let Some((progress, Some(file_id))) = &*slot.borrow()
            && let Some(active) = progress.state.lock().unwrap().active.get_mut(file_id)
        {
            active.bytes = bytes;
        }
    });
}
pub fn measure<T>(name: &'static str, f: impl FnOnce() -> T) -> T {
    let current = CURRENT.with(|slot| slot.borrow().clone());
    if let Some((progress, Some(file_id))) = &current
        && let Some(active) = progress.state.lock().unwrap().active.get_mut(file_id)
    {
        active.phase = name;
        active.phase_started = Instant::now();
    }
    let start = Instant::now();
    let result = f();
    if let Some((progress, _)) = current
        && progress.profile.load(Ordering::Relaxed)
    {
        let ms = start.elapsed().as_secs_f64() * 1000.;
        let mut s = progress.state.lock().unwrap();
        let timing = s.phases.entry(name.to_owned()).or_default();
        timing.calls += 1;
        timing.total_ms += ms;
        timing.max_ms = timing.max_ms.max(ms);
    }
    result
}

fn phase_label(name: &str) -> &str {
    match name {
        "sidediff.difft" => "Comparing syntax",
        "sidediff.read_bytes" | "sidediff.gix_blob" => "Reading content",
        "sidediff.json_decode" | "sidediff.decode_text" => "Decoding diff",
        "sidediff.build_view" => "Building view",
        "kat.prepare" | "kat.split_spans" => "Highlighting",
        other => other,
    }
}
