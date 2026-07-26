use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use crossbeam_channel::after;
use crossbeam_channel::never;
use crossbeam_channel::select;
use crossbeam_channel::unbounded;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use nucleo::Config;
use nucleo::Injector;
use nucleo::Matcher;
use nucleo::Nucleo;
use nucleo::Utf32String;
use nucleo::pattern::CaseMatching;
use nucleo::pattern::Normalization;
use serde::Serialize;
#[cfg(target_os = "linux")]
use std::collections::HashSet;
#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::num::NonZero;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use tokio::process::Command;

#[cfg(test)]
use nucleo::Utf32Str;
#[cfg(test)]
use nucleo::pattern::AtomKind;
#[cfg(test)]
use nucleo::pattern::Pattern;

mod cli;

pub use cli::Cli;

/// A single match result returned from the search.
///
/// * `score` – Relevance score returned by `nucleo`.
/// * `path`  – Path to the matched entry (file or directory), relative to the
///   search directory.
/// * `match_type` – Whether this match is a file or directory.
/// * `indices` – Optional list of character indices that matched the query.
///   These are only filled when the caller of [`run`] sets
///   `options.compute_indices` to `true`. The indices vector follows the
///   guidance from `nucleo::pattern::Pattern::indices`: they are
///   unique and sorted in ascending order so that callers can use
///   them directly for highlighting.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FileMatch {
    pub score: u32,
    pub path: PathBuf,
    pub match_type: MatchType,
    pub root: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indices: Option<Vec<u32>>, // Sorted & deduplicated when present
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MatchType {
    File,
    Directory,
}

impl FileMatch {
    pub fn full_path(&self) -> PathBuf {
        self.root.join(&self.path)
    }
}

/// Returns the final path component for a matched path, falling back to the full path.
pub fn file_name_from_path(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

#[derive(Debug)]
pub struct FileSearchResults {
    pub matches: Vec<FileMatch>,
    pub total_match_count: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct FileSearchSnapshot {
    pub query: String,
    pub matches: Vec<FileMatch>,
    pub total_match_count: usize,
    pub scanned_file_count: usize,
    pub walk_complete: bool,
}

#[derive(Debug, Clone)]
pub struct FileSearchOptions {
    pub limit: NonZero<usize>,
    pub exclude: Vec<String>,
    pub threads: NonZero<usize>,
    pub compute_indices: bool,
    /// Toggle ignore-file processing in the walker.
    ///
    /// When enabled, `.gitignore` files are scoped by
    /// `WalkBuilder::require_git(true)`, so they are honored only when the
    /// traversed path is inside a git repository. When disabled, the walker
    /// turns off `.gitignore`, git-global/exclude rules, `.ignore`, and
    /// parent-directory ignore scanning.
    pub respect_gitignore: bool,
}

impl Default for FileSearchOptions {
    fn default() -> Self {
        Self {
            #[expect(clippy::unwrap_used)]
            limit: NonZero::new(20).unwrap(),
            exclude: Vec::new(),
            #[expect(clippy::unwrap_used)]
            threads: NonZero::new(2).unwrap(),
            compute_indices: false,
            respect_gitignore: true,
        }
    }
}

pub trait SessionReporter: Send + Sync + 'static {
    /// Called when the debounced top-N changes.
    fn on_update(&self, snapshot: &FileSearchSnapshot);

    /// Called when the session becomes idle or is cancelled. Guaranteed to be called at least once per update_query.
    fn on_complete(&self);
}

pub struct FileSearchSession {
    inner: Arc<SessionInner>,
}

impl FileSearchSession {
    /// Update the query. This should be cheap relative to re-walking.
    pub fn update_query(&self, pattern_text: &str) {
        let _ = self
            .inner
            .work_tx
            .send(WorkSignal::QueryUpdated(pattern_text.to_string()));
    }
}

impl Drop for FileSearchSession {
    fn drop(&mut self) {
        self.inner.shutdown.store(true, Ordering::Relaxed);
        let _ = self.inner.work_tx.send(WorkSignal::Shutdown);
    }
}

pub fn create_session(
    search_directories: Vec<PathBuf>,
    options: FileSearchOptions,
    reporter: Arc<dyn SessionReporter>,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> anyhow::Result<FileSearchSession> {
    let FileSearchOptions {
        limit,
        exclude,
        threads,
        compute_indices,
        respect_gitignore,
    } = options;

    let Some(primary_search_directory) = search_directories.first() else {
        anyhow::bail!("at least one search directory is required");
    };
    let override_matcher = build_override_matcher(primary_search_directory, &exclude)?;
    let (work_tx, work_rx) = unbounded();

    let notify_tx = work_tx.clone();
    let notify = Arc::new(move || {
        let _ = notify_tx.send(WorkSignal::NucleoNotify);
    });
    let nucleo = Nucleo::new(
        Config::DEFAULT.match_paths(),
        notify,
        Some(threads.get()),
        1,
    );
    let injector = nucleo.injector();

    let cancelled = cancel_flag.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));

    let match_type_tracker = Arc::new(MatchTypeTracker::new());
    let inner = Arc::new(SessionInner {
        search_directories,
        limit: limit.get(),
        threads: threads.get(),
        compute_indices,
        respect_gitignore,
        cancelled,
        shutdown: Arc::new(AtomicBool::new(false)),
        reporter,
        match_type_tracker,
        work_tx,
    });

    let matcher_inner = inner.clone();
    thread::spawn(move || matcher_worker(matcher_inner, work_rx, nucleo));

    let walker_inner = inner.clone();
    thread::spawn(move || walker_worker(walker_inner, override_matcher, injector));

    Ok(FileSearchSession { inner })
}

pub trait Reporter {
    fn report_match(&self, file_match: &FileMatch);
    fn warn_matches_truncated(&self, total_match_count: usize, shown_match_count: usize);
    fn warn_no_search_pattern(&self, search_directory: &Path);
}

pub async fn run_main<T: Reporter>(
    Cli {
        pattern,
        limit,
        cwd,
        compute_indices,
        json: _,
        exclude,
        threads,
    }: Cli,
    reporter: T,
) -> anyhow::Result<()> {
    let search_directory = match cwd {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    };
    let pattern_text = match pattern {
        Some(pattern) => pattern,
        None => {
            reporter.warn_no_search_pattern(&search_directory);
            #[cfg(unix)]
            Command::new("ls")
                .arg("-al")
                .current_dir(search_directory)
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status()
                .await?;
            #[cfg(windows)]
            {
                Command::new("cmd")
                    .arg("/c")
                    .arg(search_directory)
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .status()
                    .await?;
            }
            return Ok(());
        }
    };

    let FileSearchResults {
        total_match_count,
        matches,
    } = run(
        &pattern_text,
        vec![search_directory.to_path_buf()],
        FileSearchOptions {
            limit,
            exclude,
            threads,
            compute_indices,
            respect_gitignore: true,
        },
        /*cancel_flag*/ None,
    )?;
    let match_count = matches.len();
    let matches_truncated = total_match_count > match_count;

    for file_match in matches {
        reporter.report_match(&file_match);
    }
    if matches_truncated {
        reporter.warn_matches_truncated(total_match_count, match_count);
    }

    Ok(())
}

/// The worker threads will periodically check `cancel_flag` to see if they
/// should stop processing files.
pub fn run(
    pattern_text: &str,
    roots: Vec<PathBuf>,
    options: FileSearchOptions,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> anyhow::Result<FileSearchResults> {
    let reporter = Arc::new(RunReporter::default());
    let session = create_session(roots, options, reporter.clone(), cancel_flag)?;

    session.update_query(pattern_text);

    let snapshot = reporter.wait_for_complete();
    Ok(FileSearchResults {
        matches: snapshot.matches,
        total_match_count: snapshot.total_match_count,
    })
}

/// Sort matches in-place by descending score, then ascending path.
#[cfg(test)]
fn sort_matches(matches: &mut [(u32, String)]) {
    matches.sort_by(cmp_by_score_desc_then_path_asc::<(u32, String), _, _>(
        |t| t.0,
        |t| t.1.as_str(),
    ));
}

/// Returns a comparator closure suitable for `slice.sort_by(...)` that orders
/// items by descending score and then ascending path using the provided accessors.
pub fn cmp_by_score_desc_then_path_asc<T, FScore, FPath>(
    score_of: FScore,
    path_of: FPath,
) -> impl FnMut(&T, &T) -> std::cmp::Ordering
where
    FScore: Fn(&T) -> u32,
    FPath: Fn(&T) -> &str,
{
    use std::cmp::Ordering;
    move |a, b| match score_of(b).cmp(&score_of(a)) {
        Ordering::Equal => path_of(a).cmp(path_of(b)),
        other => other,
    }
}

#[cfg(test)]
fn create_pattern(pattern: &str) -> Pattern {
    Pattern::new(
        pattern,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
    )
}

const UNKNOWN_MATCH_TYPE: u8 = 0;
const FILE_MATCH_TYPE: u8 = 1;
const DIRECTORY_MATCH_TYPE: u8 = 2;

struct SearchItem {
    full_path: Arc<str>,
    cached_match_type: AtomicU8,
    // The walk-time type is not usable until a snapshot has classified this
    // item after its parent directory became watched.
    cache_is_current: AtomicBool,
    cacheable_match_type: bool,
}

impl SearchItem {
    fn new(full_path: Arc<str>, match_type: Option<MatchType>, cacheable_match_type: bool) -> Self {
        Self {
            full_path,
            cached_match_type: AtomicU8::new(
                match_type.map_or(UNKNOWN_MATCH_TYPE, encode_match_type),
            ),
            cache_is_current: AtomicBool::new(false),
            cacheable_match_type,
        }
    }

    fn cached_match_type(&self) -> Option<MatchType> {
        (self.cacheable_match_type && self.cache_is_current.load(Ordering::Relaxed))
            .then(|| decode_match_type(self.cached_match_type.load(Ordering::Relaxed)))
            .flatten()
    }

    fn update_cached_match_type(&self, match_type: MatchType) {
        if self.cacheable_match_type {
            self.cached_match_type
                .store(encode_match_type(match_type), Ordering::Relaxed);
            self.cache_is_current.store(true, Ordering::Relaxed);
        }
    }
}

fn encode_match_type(match_type: MatchType) -> u8 {
    match match_type {
        MatchType::File => FILE_MATCH_TYPE,
        MatchType::Directory => DIRECTORY_MATCH_TYPE,
    }
}

fn decode_match_type(encoded: u8) -> Option<MatchType> {
    match encoded {
        FILE_MATCH_TYPE => Some(MatchType::File),
        DIRECTORY_MATCH_TYPE => Some(MatchType::Directory),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
const MAX_MATCH_TYPE_WATCHES: usize = 1024;

#[cfg(target_os = "linux")]
struct MatchTypeTracker {
    fd: libc::c_int,
    watched_directories: Mutex<HashSet<PathBuf>>,
    dirty: AtomicBool,
}

#[cfg(target_os = "linux")]
impl MatchTypeTracker {
    fn new() -> Self {
        // SAFETY: inotify_init1 has no pointer arguments and returns an owned file descriptor.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        Self {
            fd,
            watched_directories: Mutex::new(HashSet::new()),
            dirty: AtomicBool::new(fd < 0),
        }
    }

    fn watch_ancestors(&self, path: &Path) {
        // A retained search root can be replaced by renaming any lexical
        // ancestor, so stop only at the filesystem root.
        let mut directory = path.parent();
        while let Some(current) = directory {
            self.watch_directory(current);
            if self.dirty.load(Ordering::Relaxed) {
                return;
            }
            directory = current.parent();
        }
    }

    fn watch_directory(&self, directory: &Path) {
        if self.dirty.load(Ordering::Relaxed) {
            return;
        }

        let directory_path = directory.to_path_buf();
        let mut watched_directories = self
            .watched_directories
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if watched_directories.contains(&directory_path) {
            return;
        }
        if watched_directories.len() >= MAX_MATCH_TYPE_WATCHES {
            // Preserve fresh classifications rather than consuming unbounded
            // per-session inotify resources.
            self.dirty.store(true, Ordering::Relaxed);
            return;
        }
        let Ok(directory) = CString::new(directory_path.as_os_str().as_bytes()) else {
            self.dirty.store(true, Ordering::Relaxed);
            return;
        };
        let mask = libc::IN_ATTRIB
            | libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO;
        // SAFETY: fd is owned by this tracker, and directory is a NUL-terminated path string.
        let watch = unsafe { libc::inotify_add_watch(self.fd, directory.as_ptr(), mask) };
        if watch < 0 {
            self.dirty.store(true, Ordering::Relaxed);
            return;
        }
        watched_directories.insert(directory_path);
    }

    fn can_use_cached_match_types(&self) -> bool {
        if self.dirty.load(Ordering::Relaxed) {
            return false;
        }
        let _watched_directories = self
            .watched_directories
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut buffer = [0_u8; 4096];
        loop {
            // SAFETY: fd is a live nonblocking inotify descriptor and buffer is writable.
            let read = unsafe { libc::read(self.fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if read > 0 {
                self.dirty.store(true, Ordering::Relaxed);
                return false;
            }
            if read == 0 {
                self.dirty.store(true, Ordering::Relaxed);
                return false;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return true;
            }
            self.dirty.store(true, Ordering::Relaxed);
            return false;
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for MatchTypeTracker {
    fn drop(&mut self) {
        if self.fd >= 0 {
            // SAFETY: fd is owned by this tracker and is closed exactly once on drop.
            unsafe {
                libc::close(self.fd);
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
struct MatchTypeTracker;

#[cfg(not(target_os = "linux"))]
impl MatchTypeTracker {
    fn new() -> Self {
        Self
    }

    fn watch_ancestors(&self, _path: &Path) {}

    fn can_use_cached_match_types(&self) -> bool {
        false
    }
}

struct SessionInner {
    search_directories: Vec<PathBuf>,
    limit: usize,
    threads: usize,
    compute_indices: bool,
    respect_gitignore: bool,
    cancelled: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    reporter: Arc<dyn SessionReporter>,
    match_type_tracker: Arc<MatchTypeTracker>,
    work_tx: Sender<WorkSignal>,
}

enum WorkSignal {
    QueryUpdated(String),
    NucleoNotify,
    WalkComplete,
    Shutdown,
}

fn build_override_matcher(
    search_directory: &Path,
    exclude: &[String],
) -> anyhow::Result<Option<ignore::overrides::Override>> {
    if exclude.is_empty() {
        return Ok(None);
    }
    let mut override_builder = OverrideBuilder::new(search_directory);
    for exclude in exclude {
        let exclude_pattern = format!("!{exclude}");
        override_builder.add(&exclude_pattern)?;
    }
    let matcher = override_builder.build()?;
    Ok(Some(matcher))
}

fn get_file_path<'a>(path: &'a Path, search_directories: &[PathBuf]) -> Option<(usize, &'a str)> {
    let mut best_match: Option<(usize, &Path)> = None;
    for (idx, root) in search_directories.iter().enumerate() {
        if let Ok(rel_path) = path.strip_prefix(root) {
            let root_depth = root.components().count();
            match best_match {
                Some((best_idx, _))
                    if search_directories[best_idx].components().count() >= root_depth => {}
                _ => {
                    best_match = Some((idx, rel_path));
                }
            }
        }
    }

    let (root_idx, rel_path) = best_match?;
    rel_path.to_str().map(|p| (root_idx, p))
}

/// Walks the search directories and feeds discovered paths into `nucleo`
/// via the injector.
///
/// The walker uses `require_git(true)` to match git's own ignore semantics:
/// git never reads `.gitignore` files from directories above the repository
/// root. Without this flag, the `ignore` crate reads `.gitignore` files from
/// *all* ancestor directories—a deliberate divergence from git intended for
/// non-git use cases—allowing a broad parent ignore (e.g. `~/.gitignore`
/// containing `*`) to silently suppress every file in the walk.
///
/// When `respect_gitignore` is `false`, all git-related ignore processing is
/// disabled regardless of this flag.
fn walker_worker(
    inner: Arc<SessionInner>,
    override_matcher: Option<ignore::overrides::Override>,
    injector: Injector<SearchItem>,
) {
    let Some(first_root) = inner.search_directories.first() else {
        let _ = inner.work_tx.send(WorkSignal::WalkComplete);
        return;
    };

    let mut walk_builder = WalkBuilder::new(first_root);
    for root in inner.search_directories.iter().skip(1) {
        walk_builder.add(root);
    }
    walk_builder
        .threads(inner.threads)
        // Allow hidden entries.
        .hidden(false)
        // Follow symlinks to search their contents.
        .follow_links(true)
        // Keep ignore behavior aligned with git repositories: only apply
        // gitignore rules when a git context exists.
        .require_git(true);
    if !inner.respect_gitignore {
        walk_builder
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .ignore(false)
            .parents(false);
    }
    if let Some(override_matcher) = override_matcher {
        walk_builder.overrides(override_matcher);
    }

    let walker = walk_builder.build_parallel();

    walker.run(|| {
        const CHECK_INTERVAL: usize = 1024;
        let mut n = 0;
        let search_directories = inner.search_directories.clone();
        let injector = injector.clone();
        let cancelled = inner.cancelled.clone();
        let shutdown = inner.shutdown.clone();

        Box::new(move |entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => return ignore::WalkState::Continue,
            };
            let path = entry.path();
            let Some(full_path) = path.to_str() else {
                return ignore::WalkState::Continue;
            };
            let is_symlink = entry.path_is_symlink();
            let match_type = if is_symlink {
                // `file_type` reports the link itself, while the existing
                // `Path::is_dir` behavior follows it. Keep symlink
                // classification deferred to preserve that behavior.
                None
            } else {
                entry.file_type().map(|file_type| {
                    if file_type.is_dir() {
                        MatchType::Directory
                    } else {
                        MatchType::File
                    }
                })
            };
            if let Some((_, relative_path)) = get_file_path(path, &search_directories) {
                injector.push(
                    SearchItem::new(Arc::from(full_path), match_type, !is_symlink),
                    |_, cols| {
                        cols[0] = Utf32String::from(relative_path);
                    },
                );
            }
            n += 1;
            if n >= CHECK_INTERVAL {
                if cancelled.load(Ordering::Relaxed) || shutdown.load(Ordering::Relaxed) {
                    return ignore::WalkState::Quit;
                }
                n = 0;
            }
            ignore::WalkState::Continue
        })
    });
    let _ = inner.work_tx.send(WorkSignal::WalkComplete);
}

fn matcher_worker(
    inner: Arc<SessionInner>,
    work_rx: Receiver<WorkSignal>,
    mut nucleo: Nucleo<SearchItem>,
) -> anyhow::Result<()> {
    const TICK_TIMEOUT_MS: u64 = 10;
    let config = Config::DEFAULT.match_paths();
    let mut indices_matcher = inner.compute_indices.then(|| Matcher::new(config.clone()));
    let cancel_requested = || inner.cancelled.load(Ordering::Relaxed);
    let shutdown_requested = || inner.shutdown.load(Ordering::Relaxed);

    let mut last_query = String::new();
    let mut next_notify = never();
    let mut will_notify = false;
    let mut walk_complete = false;

    loop {
        select! {
            recv(work_rx) -> signal => {
                let Ok(signal) = signal else {
                    break;
                };
                match signal {
                    WorkSignal::QueryUpdated(query) => {
                        let append = query.starts_with(&last_query);
                        nucleo.pattern.reparse(
                            0,
                            &query,
                            CaseMatching::Ignore,
                            Normalization::Smart,
                            append,
                        );
                        last_query = query;
                        will_notify = true;
                        next_notify = after(Duration::from_millis(0));
                    }
                    WorkSignal::NucleoNotify => {
                        if !will_notify {
                            will_notify = true;
                            next_notify = after(Duration::from_millis(TICK_TIMEOUT_MS));
                        }
                    }
                    WorkSignal::WalkComplete => {
                        walk_complete = true;
                        if !will_notify {
                            will_notify = true;
                            next_notify = after(Duration::from_millis(0));
                        }
                    }
                    WorkSignal::Shutdown => {
                        break;
                    }
                }
            }
            recv(next_notify) -> _ => {
                will_notify = false;
                let status = nucleo.tick(TICK_TIMEOUT_MS);
                if status.changed {
                    let snapshot = nucleo.snapshot();
                    let limit = inner.limit.min(snapshot.matched_item_count() as usize);
                    for match_ in snapshot.matches().iter().take(limit) {
                        let Some(item) = snapshot.get_item(match_.idx) else {
                            continue;
                        };
                        if item.data.cacheable_match_type {
                            let path = Path::new(item.data.full_path.as_ref());
                            inner.match_type_tracker.watch_ancestors(path);
                        }
                    }
                    let use_cached_match_types = inner.match_type_tracker.can_use_cached_match_types();
                    let pattern = snapshot.pattern().column_pattern(0);
                    let matches: Vec<_> = snapshot
                        .matches()
                        .iter()
                        .take(limit)
                        .filter_map(|match_| {
                            let item = snapshot.get_item(match_.idx)?;
                            let full_path = item.data.full_path.as_ref();
                            let (root_idx, relative_path) = get_file_path(Path::new(full_path), &inner.search_directories)?;
                            let indices = if let Some(indices_matcher) = indices_matcher.as_mut() {
                                let mut idx_vec = Vec::<u32>::new();
                                let haystack = item.matcher_columns[0].slice(..);
                                let _ = pattern.indices(haystack, indices_matcher, &mut idx_vec);
                                idx_vec.sort_unstable();
                                idx_vec.dedup();
                                Some(idx_vec)
                            } else {
                                None
                            };
                            let match_type = if use_cached_match_types {
                                item.data.cached_match_type()
                            } else {
                                None
                            }
                            .unwrap_or_else(|| {
                                let match_type = if Path::new(full_path).is_dir() {
                                    MatchType::Directory
                                } else {
                                    MatchType::File
                                };
                                item.data.update_cached_match_type(match_type);
                                match_type
                            });
                            Some(FileMatch {
                                score: match_.score,
                                path: PathBuf::from(relative_path),
                                match_type,
                                root: inner.search_directories[root_idx].clone(),
                                indices,
                            })
                        })
                        .collect();

                    let snapshot = FileSearchSnapshot {
                        query: last_query.clone(),
                        matches,
                        total_match_count: snapshot.matched_item_count() as usize,
                        scanned_file_count: snapshot.item_count() as usize,
                        walk_complete,
                    };
                    inner.reporter.on_update(&snapshot);
                }
                if !status.running && walk_complete {
                    inner.reporter.on_complete();
                }
            }
            default(Duration::from_millis(100)) => {
                // Occasionally check the cancel flag.
            }
        }

        if cancel_requested() || shutdown_requested() {
            break;
        }
    }

    // If we cancelled or otherwise exited the loop, make sure the reporter is notified.
    inner.reporter.on_complete();

    Ok(())
}

#[derive(Default)]
struct RunReporter {
    snapshot: RwLock<FileSearchSnapshot>,
    completed: (Condvar, Mutex<bool>),
}

impl SessionReporter for RunReporter {
    fn on_update(&self, snapshot: &FileSearchSnapshot) {
        #[allow(clippy::unwrap_used)]
        let mut guard = self.snapshot.write().unwrap();
        *guard = snapshot.clone();
    }

    fn on_complete(&self) {
        let (cv, mutex) = &self.completed;
        #[allow(clippy::unwrap_used)]
        let mut completed = mutex.lock().unwrap();
        *completed = true;
        cv.notify_all();
    }
}

impl RunReporter {
    fn wait_for_complete(&self) -> FileSearchSnapshot {
        let (cv, mutex) = &self.completed;
        #[allow(clippy::unwrap_used)]
        let mut completed = mutex.lock().unwrap();
        while !*completed {
            #[allow(clippy::unwrap_used)]
            {
                completed = cv.wait(completed).unwrap();
            }
        }
        #[allow(clippy::unwrap_used)]
        self.snapshot.read().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::sync::Arc;
    use std::sync::Condvar;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;
    use tempfile::TempDir;

    #[test]
    fn verify_score_is_none_for_non_match() {
        let mut utf32buf = Vec::<char>::new();
        let line = "hello";
        let mut matcher = Matcher::new(Config::DEFAULT);
        let haystack: Utf32Str<'_> = Utf32Str::new(line, &mut utf32buf);
        let pattern = create_pattern("zzz");
        let score = pattern.score(haystack, &mut matcher);
        assert_eq!(score, None);
    }

    #[test]
    fn tie_breakers_sort_by_path_when_scores_equal() {
        let mut matches = vec![
            (100, "b_path".to_string()),
            (100, "a_path".to_string()),
            (90, "zzz".to_string()),
        ];

        sort_matches(&mut matches);

        // Highest score first; ties broken alphabetically.
        let expected = vec![
            (100, "a_path".to_string()),
            (100, "b_path".to_string()),
            (90, "zzz".to_string()),
        ];

        assert_eq!(matches, expected);
    }

    #[test]
    fn file_name_from_path_uses_basename() {
        assert_eq!(file_name_from_path("foo/bar.txt"), "bar.txt");
    }

    #[test]
    fn file_name_from_path_falls_back_to_full_path() {
        assert_eq!(file_name_from_path(""), "");
    }

    #[derive(Default)]
    struct RecordingReporter {
        updates: Mutex<Vec<FileSearchSnapshot>>,
        complete_times: Mutex<Vec<Instant>>,
        complete_cv: Condvar,
        update_cv: Condvar,
    }

    impl RecordingReporter {
        fn wait_until<T, F>(
            &self,
            mutex: &Mutex<T>,
            cv: &Condvar,
            timeout: Duration,
            mut predicate: F,
        ) -> bool
        where
            F: FnMut(&T) -> bool,
        {
            let deadline = Instant::now() + timeout;
            let mut state = mutex.lock().unwrap();
            loop {
                if predicate(&state) {
                    return true;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return false;
                }
                let (next_state, wait_result) = cv.wait_timeout(state, remaining).unwrap();
                state = next_state;
                if wait_result.timed_out() {
                    return predicate(&state);
                }
            }
        }

        fn wait_for_complete(&self, timeout: Duration) -> bool {
            self.wait_until(
                &self.complete_times,
                &self.complete_cv,
                timeout,
                |completes| !completes.is_empty(),
            )
        }
        fn clear(&self) {
            self.updates.lock().unwrap().clear();
            self.complete_times.lock().unwrap().clear();
        }

        fn updates(&self) -> Vec<FileSearchSnapshot> {
            self.updates.lock().unwrap().clone()
        }

        fn wait_for_updates_at_least(&self, min_len: usize, timeout: Duration) -> bool {
            self.wait_until(&self.updates, &self.update_cv, timeout, |updates| {
                updates.len() >= min_len
            })
        }

        fn snapshot(&self) -> FileSearchSnapshot {
            self.updates
                .lock()
                .unwrap()
                .last()
                .cloned()
                .unwrap_or_default()
        }
    }

    impl SessionReporter for RecordingReporter {
        fn on_update(&self, snapshot: &FileSearchSnapshot) {
            let mut updates = self.updates.lock().unwrap();
            updates.push(snapshot.clone());
            self.update_cv.notify_all();
        }

        fn on_complete(&self) {
            {
                let mut complete_times = self.complete_times.lock().unwrap();
                complete_times.push(Instant::now());
            }
            self.complete_cv.notify_all();
        }
    }

    fn create_temp_tree(file_count: usize) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..file_count {
            let path = dir.path().join(format!("file-{i:04}.txt"));
            fs::write(path, format!("contents {i}")).unwrap();
        }
        dir
    }

    #[test]
    fn session_scanned_file_count_is_monotonic_across_queries() {
        let dir = create_temp_tree(/*file_count*/ 200);
        let reporter = Arc::new(RecordingReporter::default());
        let session = create_session(
            vec![dir.path().to_path_buf()],
            FileSearchOptions::default(),
            reporter.clone(),
            /*cancel_flag*/ None,
        )
        .expect("session");

        session.update_query("file-00");
        thread::sleep(Duration::from_millis(20));
        let first_snapshot = reporter.snapshot();
        session.update_query("file-01");
        thread::sleep(Duration::from_millis(20));
        let second_snapshot = reporter.snapshot();
        let _ = reporter.wait_for_complete(Duration::from_secs(5));
        let completed_snapshot = reporter.snapshot();

        assert!(second_snapshot.scanned_file_count >= first_snapshot.scanned_file_count);
        assert!(completed_snapshot.scanned_file_count >= second_snapshot.scanned_file_count);
    }

    #[test]
    fn session_streams_updates_before_walk_complete() {
        let dir = create_temp_tree(/*file_count*/ 600);
        let reporter = Arc::new(RecordingReporter::default());
        let session = create_session(
            vec![dir.path().to_path_buf()],
            FileSearchOptions::default(),
            reporter.clone(),
            /*cancel_flag*/ None,
        )
        .expect("session");

        session.update_query("file-0");
        let completed = reporter.wait_for_complete(Duration::from_secs(5));

        assert!(completed);
        let updates = reporter.updates();
        assert!(updates.iter().any(|snapshot| !snapshot.walk_complete));
    }

    #[test]
    fn session_accepts_query_updates_after_walk_complete() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha.txt"), "alpha").unwrap();
        fs::write(dir.path().join("beta.txt"), "beta").unwrap();
        let reporter = Arc::new(RecordingReporter::default());
        let session = create_session(
            vec![dir.path().to_path_buf()],
            FileSearchOptions::default(),
            reporter.clone(),
            /*cancel_flag*/ None,
        )
        .expect("session");

        session.update_query("alpha");
        assert!(reporter.wait_for_complete(Duration::from_secs(5)));
        let updates_before = reporter.updates().len();

        session.update_query("beta");
        assert!(reporter.wait_for_updates_at_least(updates_before + 1, Duration::from_secs(5),));

        let updates = reporter.updates();
        let last_update = updates.last().cloned().expect("update");
        assert!(
            last_update
                .matches
                .iter()
                .any(|file_match| file_match.path.to_string_lossy().contains("beta.txt"))
        );
    }

    #[test]
    fn session_emits_updates_when_query_changes_and_refreshes_match_type_after_entry_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join("matched-entry");
        fs::write(&entry, "fixture").unwrap();
        let reporter = Arc::new(RecordingReporter::default());
        let session = create_session(
            vec![dir.path().to_path_buf()],
            FileSearchOptions::default(),
            reporter.clone(),
            /*cancel_flag*/ None,
        )
        .expect("session");

        session.update_query("matched");
        assert!(reporter.wait_for_complete(Duration::from_secs(5)));
        reporter.clear();

        fs::remove_file(&entry).unwrap();
        fs::create_dir(&entry).unwrap();
        session.update_query("match");
        assert!(reporter.wait_until(
            &reporter.updates,
            &reporter.update_cv,
            Duration::from_secs(5),
            |updates| updates.iter().any(|snapshot| snapshot.query == "match"),
        ));

        let update = reporter
            .updates()
            .into_iter()
            .rev()
            .find(|snapshot| snapshot.query == "match")
            .expect("replacement update");
        let match_type = update
            .matches
            .iter()
            .find(|file_match| file_match.path == Path::new("matched-entry"))
            .map(|file_match| file_match.match_type);
        assert_eq!(match_type, Some(MatchType::Directory));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn session_emits_updates_when_query_changes_and_reclassifies_unwatched_entries() {
        let dir = tempfile::tempdir().unwrap();
        let watched_parent = dir.path().join("watched-parent");
        let unwatched_parent = dir.path().join("unwatched-parent");
        fs::create_dir(&watched_parent).unwrap();
        fs::create_dir(&unwatched_parent).unwrap();
        fs::write(watched_parent.join("first-entry"), "fixture").unwrap();
        let changed = unwatched_parent.join("changed-entry");
        fs::write(&changed, "fixture").unwrap();
        let reporter = Arc::new(RecordingReporter::default());
        let session = create_session(
            vec![dir.path().to_path_buf()],
            FileSearchOptions::default(),
            reporter.clone(),
            /*cancel_flag*/ None,
        )
        .expect("session");

        session.update_query("first");
        assert!(reporter.wait_for_complete(Duration::from_secs(5)));
        assert!(reporter.updates().iter().any(|snapshot| {
            snapshot.query == "first"
                && snapshot
                    .matches
                    .iter()
                    .any(|file_match| file_match.path == Path::new("watched-parent/first-entry"))
        }));
        reporter.clear();

        // The first query does not watch `unwatched_parent`, so this
        // replacement is not reported by its existing directory watches.
        fs::remove_file(&changed).unwrap();
        fs::create_dir(&changed).unwrap();
        session.update_query("changed");
        assert!(reporter.wait_until(
            &reporter.updates,
            &reporter.update_cv,
            Duration::from_secs(5),
            |updates| updates.iter().any(|snapshot| snapshot.query == "changed"),
        ));

        let update = reporter
            .updates()
            .into_iter()
            .rev()
            .find(|snapshot| snapshot.query == "changed")
            .expect("changed-entry update");
        let match_type = update
            .matches
            .iter()
            .find(|file_match| file_match.path == Path::new("unwatched-parent/changed-entry"))
            .map(|file_match| file_match.match_type);
        assert_eq!(match_type, Some(MatchType::Directory));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn session_emits_updates_when_query_changes_and_reclassifies_entries_after_root_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let root = base.join("root");
        let entry = root.join("entry");
        fs::create_dir_all(&entry).unwrap();
        let reporter = Arc::new(RecordingReporter::default());
        let session = create_session(
            vec![root.clone()],
            FileSearchOptions::default(),
            reporter.clone(),
            /*cancel_flag*/ None,
        )
        .expect("session");

        session.update_query("entry");
        assert!(reporter.wait_for_complete(Duration::from_secs(5)));
        assert!(reporter.updates().iter().any(|snapshot| {
            snapshot.query == "entry"
                && snapshot.matches.iter().any(|file_match| {
                    file_match.path == Path::new("entry")
                        && file_match.match_type == MatchType::Directory
                })
        }));
        reporter.clear();

        // This changes the root's path identity through an ancestor that is
        // outside the root itself.
        fs::rename(&base, dir.path().join("base-old")).unwrap();
        fs::create_dir_all(&root).unwrap();
        fs::write(&entry, "fixture").unwrap();
        session.update_query("entr");
        assert!(reporter.wait_until(
            &reporter.updates,
            &reporter.update_cv,
            Duration::from_secs(5),
            |updates| updates.iter().any(|snapshot| snapshot.query == "entr"),
        ));

        let update = reporter
            .updates()
            .into_iter()
            .rev()
            .find(|snapshot| snapshot.query == "entr")
            .expect("replacement-root update");
        let match_type = update
            .matches
            .iter()
            .find(|file_match| file_match.path == Path::new("entry"))
            .map(|file_match| file_match.match_type);
        assert_eq!(match_type, Some(MatchType::File));
    }

    #[test]
    fn session_emits_complete_when_query_changes_with_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha.txt"), "alpha").unwrap();
        fs::write(dir.path().join("beta.txt"), "beta").unwrap();
        let reporter = Arc::new(RecordingReporter::default());
        let session = create_session(
            vec![dir.path().to_path_buf()],
            FileSearchOptions::default(),
            reporter.clone(),
            /*cancel_flag*/ None,
        )
        .expect("session");

        session.update_query("asdf");
        assert!(reporter.wait_for_complete(Duration::from_secs(5)));

        let completed_snapshot = reporter.snapshot();
        assert_eq!(completed_snapshot.matches, Vec::new());
        assert_eq!(completed_snapshot.total_match_count, 0);

        reporter.clear();

        session.update_query("asdfa");
        assert!(reporter.wait_for_complete(Duration::from_secs(5)));
        assert!(!reporter.updates().is_empty());
    }

    #[test]
    fn dropping_session_does_not_cancel_siblings_with_shared_cancel_flag() {
        let root_a = create_temp_tree(/*file_count*/ 200);
        let root_b = create_temp_tree(/*file_count*/ 4_000);
        let cancel_flag = Arc::new(AtomicBool::new(false));

        let reporter_a = Arc::new(RecordingReporter::default());
        let session_a = create_session(
            vec![root_a.path().to_path_buf()],
            FileSearchOptions::default(),
            reporter_a,
            Some(cancel_flag.clone()),
        )
        .expect("session_a");

        let reporter_b = Arc::new(RecordingReporter::default());
        let session_b = create_session(
            vec![root_b.path().to_path_buf()],
            FileSearchOptions::default(),
            reporter_b.clone(),
            Some(cancel_flag),
        )
        .expect("session_b");

        session_a.update_query("file-0");
        session_b.update_query("file-1");

        thread::sleep(Duration::from_millis(5));
        drop(session_a);

        let completed = reporter_b.wait_for_complete(Duration::from_secs(5));
        assert_eq!(completed, true);
    }

    #[test]
    fn session_emits_updates_when_query_changes() {
        let dir = create_temp_tree(/*file_count*/ 200);
        let reporter = Arc::new(RecordingReporter::default());
        let session = create_session(
            vec![dir.path().to_path_buf()],
            FileSearchOptions::default(),
            reporter.clone(),
            /*cancel_flag*/ None,
        )
        .expect("session");

        session.update_query("zzzzzzzz");
        let completed = reporter.wait_for_complete(Duration::from_secs(5));
        assert!(completed);

        reporter.clear();

        session.update_query("zzzzzzzzq");
        let completed = reporter.wait_for_complete(Duration::from_secs(5));
        assert!(completed);

        let updates = reporter.updates();
        assert_eq!(updates.len(), 1);
    }

    #[test]
    fn run_returns_matches_for_query() {
        let dir = create_temp_tree(/*file_count*/ 40);
        let options = FileSearchOptions {
            limit: NonZero::new(20).unwrap(),
            exclude: Vec::new(),
            threads: NonZero::new(2).unwrap(),
            compute_indices: false,
            respect_gitignore: true,
        };
        let results = run(
            "file-000",
            vec![dir.path().to_path_buf()],
            options,
            /*cancel_flag*/ None,
        )
        .expect("run ok");

        assert!(!results.matches.is_empty());
        assert!(results.total_match_count >= results.matches.len());
        assert!(
            results
                .matches
                .iter()
                .any(|m| m.path.to_string_lossy().contains("file-0000.txt"))
        );
    }

    #[test]
    fn run_returns_directory_matches_for_query() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("docs/guides")).unwrap();
        fs::write(dir.path().join("docs/guides/intro.md"), "intro").unwrap();
        fs::write(dir.path().join("docs/readme.md"), "readme").unwrap();

        let results = run(
            "guides",
            vec![dir.path().to_path_buf()],
            FileSearchOptions {
                limit: NonZero::new(20).unwrap(),
                exclude: Vec::new(),
                threads: NonZero::new(2).unwrap(),
                compute_indices: false,
                respect_gitignore: true,
            },
            /*cancel_flag*/ None,
        )
        .expect("run ok");

        assert!(results.matches.iter().any(|m| {
            m.path == std::path::Path::new("docs").join("guides")
                && m.match_type == MatchType::Directory
        }));
    }

    #[test]
    fn cancel_exits_run() {
        let dir = create_temp_tree(/*file_count*/ 200);
        let cancel_flag = Arc::new(AtomicBool::new(true));
        let search_dir = dir.path().to_path_buf();
        let options = FileSearchOptions {
            compute_indices: false,
            ..Default::default()
        };
        let (tx, rx) = std::sync::mpsc::channel();

        let handle = thread::spawn(move || {
            let result = run("file-", vec![search_dir], options, Some(cancel_flag));
            let _ = tx.send(result);
        });

        let result = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("run should exit after cancellation");
        handle.join().unwrap();

        let results = result.expect("run ok");
        assert_eq!(results.matches, Vec::new());
        assert_eq!(results.total_match_count, 0);
    }

    /// Regression test for #3493: a parent directory's `.gitignore` with `*`
    /// must not suppress files discovered inside a child "repo" directory.
    ///
    /// The fixture intentionally omits `git init` so that no `.git` directory
    /// exists. With `require_git(true)`, the walker skips all gitignore
    /// processing, making the parent's broad ignore harmless.
    #[test]
    fn parent_gitignore_outside_repo_does_not_hide_repo_files() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("home");
        let repo = parent.join("repo");
        fs::create_dir_all(repo.join(".vscode")).unwrap();

        fs::write(parent.join(".gitignore"), "*\n!.gitignore\n").unwrap();
        fs::write(
            repo.join(".gitignore"),
            ".vscode/*\n!.vscode/\n!.vscode/settings.json\n!package.json\n",
        )
        .unwrap();
        fs::write(repo.join("package.json"), "{ \"name\": \"demo\" }\n").unwrap();
        fs::write(repo.join(".vscode/settings.json"), "{ \"editor\": true }\n").unwrap();

        let respect_results = run(
            "package",
            vec![repo.clone()],
            FileSearchOptions {
                limit: NonZero::new(20).unwrap(),
                exclude: Vec::new(),
                threads: NonZero::new(2).unwrap(),
                compute_indices: false,
                respect_gitignore: true,
            },
            /*cancel_flag*/ None,
        )
        .expect("run ok");
        assert!(
            respect_results
                .matches
                .iter()
                .any(|m| m.path.as_path() == Path::new("package.json"))
        );

        let nested_file_results = run(
            "settings",
            vec![repo],
            FileSearchOptions {
                limit: NonZero::new(20).unwrap(),
                exclude: Vec::new(),
                threads: NonZero::new(2).unwrap(),
                compute_indices: false,
                respect_gitignore: true,
            },
            /*cancel_flag*/ None,
        )
        .expect("run ok");
        assert!(
            nested_file_results
                .matches
                .iter()
                .any(|m| m.path.as_path() == Path::new(".vscode/settings.json"))
        );
    }

    #[test]
    fn git_repo_still_respects_local_gitignore_when_enabled() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("home");
        let repo = parent.join("repo");
        fs::create_dir_all(repo.join(".vscode")).unwrap();

        fs::write(parent.join(".gitignore"), "*\n!.gitignore\n").unwrap();
        fs::write(
            repo.join(".gitignore"),
            ".vscode/*\n!.vscode/\n!.vscode/settings.json\n!package.json\n",
        )
        .unwrap();
        fs::write(repo.join("package.json"), "{ \"name\": \"demo\" }\n").unwrap();
        fs::write(repo.join(".vscode/settings.json"), "{ \"editor\": true }\n").unwrap();
        fs::write(
            repo.join(".vscode/extensions.json"),
            "{ \"extensions\": [] }\n",
        )
        .unwrap();

        fs::create_dir_all(repo.join(".git")).unwrap();

        let package_results = run(
            "package",
            vec![repo.clone()],
            FileSearchOptions {
                limit: NonZero::new(20).unwrap(),
                exclude: Vec::new(),
                threads: NonZero::new(2).unwrap(),
                compute_indices: false,
                respect_gitignore: true,
            },
            /*cancel_flag*/ None,
        )
        .expect("run ok");
        assert!(
            package_results
                .matches
                .iter()
                .any(|m| m.path.as_path() == Path::new("package.json"))
        );

        let ignored_results = run(
            "extensions.json",
            vec![repo.clone()],
            FileSearchOptions {
                limit: NonZero::new(20).unwrap(),
                exclude: Vec::new(),
                threads: NonZero::new(2).unwrap(),
                compute_indices: false,
                respect_gitignore: true,
            },
            /*cancel_flag*/ None,
        )
        .expect("run ok");
        assert!(
            !ignored_results
                .matches
                .iter()
                .any(|m| m.path.as_path() == Path::new(".vscode/extensions.json"))
        );

        let whitelisted_results = run(
            "settings.json",
            vec![repo],
            FileSearchOptions {
                limit: NonZero::new(20).unwrap(),
                exclude: Vec::new(),
                threads: NonZero::new(2).unwrap(),
                compute_indices: false,
                respect_gitignore: true,
            },
            /*cancel_flag*/ None,
        )
        .expect("run ok");
        assert!(
            whitelisted_results
                .matches
                .iter()
                .any(|m| m.path.as_path() == Path::new(".vscode/settings.json"))
        );
    }
}
