//! Bring the index up to date with the transcripts on disk.
//!
//! One writer at a time (a lock file); readers never wait for it. Each run compares every
//! transcript with what was recorded last time and parses only what changed: new files in
//! full, grown files from where the last run stopped, rewritten files again from the start.
//! Parsing runs in parallel; writing is single-threaded. Tantivy commits before SQLite, and
//! re-adding a record first deletes whatever an interrupted run left behind, so a crash at
//! any point is repaired by the next run.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rayon::prelude::*;

use super::fulltext::{FULLTEXT_VERSION, FullText, Merges};
use super::project::ProjectNames;
use super::source::{self, FileParse, FileState, Kind, Roots};
use super::store::{FileRow, RecordRow, SessionRow, Store};
use super::{IndexPaths, reader};

/// Indexed characters per record; the rest of a long record is readable but not searchable.
pub const INDEX_MESSAGE_BYTES: usize = 64 * 1024;
pub const INDEX_TOOL_BYTES: usize = 8 * 1024;
/// Commit at least this often during a long run, so progress survives an interruption.
const CHECKPOINT_BYTES: u64 = 1 << 30;

#[derive(Debug, Default, Clone)]
pub struct Report {
    pub files_seen: usize,
    pub files_parsed: usize,
    pub files_removed: usize,
    pub files_moved: usize,
    pub bytes_parsed: u64,
    pub records_added: u64,
    pub bad_lines: u64,
    /// Files that could not be read this time (path, error).
    pub failures: Vec<(String, String)>,
    pub rebuilt: bool,
    /// More than `Options::budget` was waiting; nothing was parsed.
    pub deferred: bool,
    /// Left for `sessift index` by a query's refresh: merges bigger than a query should wait
    /// for (see `Merges`).
    pub merges_due: bool,
    pub pending_bytes: u64,
    pub took: Duration,
}

impl Report {
    pub fn changed(&self) -> bool {
        self.files_parsed + self.files_removed + self.files_moved > 0 || self.rebuilt
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Progress {
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

#[derive(Default)]
pub struct Options<'a> {
    /// Wait for another writer instead of returning `None`.
    pub wait: bool,
    /// Throw the index away first.
    pub rebuild: bool,
    /// Parse at most this many bytes; with more waiting, return `deferred` instead.
    pub budget: Option<u64>,
    /// Refreshing for a query: run only small merges and report bigger ones as
    /// `merges_due`. Otherwise, run every merge that is due, even with nothing new to parse.
    pub in_query: bool,
    pub progress: Option<&'a (dyn Fn(Progress) + Sync)>,
}

/// A transcript file as found on disk.
#[derive(Debug, Clone)]
struct Found {
    kind: Kind,
    path: PathBuf,
    size: u64,
    mtime_ns: i64,
    inode: u64,
}

enum Plan {
    /// Parse from the start (new, rewritten, or parsed by an older parser).
    Full,
    /// Continue from the last run: bytes appended since (JSONL), or changed sessions (db).
    Update { from: u64, state: String },
}

struct Job {
    found: Found,
    prev: Option<FileRow>,
    plan: Plan,
}

impl Job {
    fn bytes(&self) -> u64 {
        match &self.plan {
            Plan::Full => self.found.size,
            Plan::Update { from, .. } => self.found.size.saturating_sub(*from),
        }
    }
}

/// Update the index. Returns `None` when another process holds the writer lock (unless
/// `opts.wait`).
pub fn refresh(paths: &IndexPaths, roots: &Roots, opts: &Options) -> Result<Option<Report>> {
    let started = Instant::now();
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("create {}", paths.dir.display()))?;
    let Some(_lock) = WriterLock::acquire(&paths.lock(), opts.wait)? else {
        return Ok(None);
    };
    let mut report = Report::default();
    let store = open_for_writing(paths, opts.rebuild, &mut report)?;

    let found = scan(roots);
    report.files_seen = found.len();
    let mut known = store.files()?;
    let mut jobs = Vec::new();
    let mut seen = HashSet::new();
    for f in found {
        seen.insert(f.path.to_string_lossy().into_owned());
        let prev = known.get(f.path.to_str().unwrap_or_default()).cloned();
        let plan = match &prev {
            None => Plan::Full,
            Some(row) => match plan_existing(&f, row, &store) {
                Some(plan) => plan,
                None => continue,
            },
        };
        jobs.push(Job {
            found: f,
            prev,
            plan,
        });
    }
    known.retain(|path, _| !seen.contains(path));
    let mut removed: Vec<FileRow> = known.into_values().collect();

    // Codex compresses finished rollouts in place: `x.jsonl` becomes `x.jsonl.zst` with the
    // same bytes inside. Keep the records instead of parsing the file again.
    let mut moves = Vec::new();
    jobs.retain(|job| {
        if job.prev.is_some() || !reader::is_compressed(&job.found.path) {
            return true;
        }
        let plain = job.found.path.with_extension("");
        let Some(at) = removed.iter().position(|r| Path::new(&r.path) == plain) else {
            return true;
        };
        let old = &removed[at];
        if old.parsed == old.size && zstd_content_size(&job.found.path) == Some(old.size) {
            moves.push((removed.remove(at), job.found.clone()));
            return false;
        }
        true
    });

    // Nothing to parse; still go on for the first build (even an empty one: queries need
    // `refreshed_ms`) and, outside a query, for merges left by queries.
    let nothing_new = jobs.is_empty()
        && removed.is_empty()
        && moves.is_empty()
        && store.meta("refreshed_ms")?.is_some();
    if nothing_new && opts.in_query {
        report.took = started.elapsed();
        return Ok(Some(report));
    }

    let bytes_total: u64 = jobs.iter().map(Job::bytes).sum();
    if opts.budget.is_some_and(|budget| bytes_total > budget) {
        report.deferred = true;
        report.pending_bytes = bytes_total;
        report.took = started.elapsed();
        return Ok(Some(report));
    }
    let ft = FullText::open_or_create(&paths.fulltext())?;
    if nothing_new && !ft.merges_due()? {
        report.took = started.elapsed();
        return Ok(Some(report));
    }
    let merges = if opts.in_query {
        Merges::Small
    } else {
        Merges::All
    };
    let big = bytes_total > 256 << 20;
    let mut writer = if big {
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .clamp(1, 6);
        ft.writer(threads * 200_000_000, threads, merges)?
    } else {
        ft.writer(64_000_000, 1, merges)?
    };
    store.begin()?;
    let result = (|| -> Result<()> {
        for (old, new) in &moves {
            store.move_file(
                old.id,
                &FileRow {
                    path: new.path.to_string_lossy().into_owned(),
                    size: new.size,
                    mtime_ns: new.mtime_ns,
                    inode: new.inode,
                    ..old.clone()
                },
            )?;
            report.files_moved += 1;
        }
        for file in &removed {
            for session in store.sessions_of_file(file.id)? {
                ft.delete_session(&writer, session.id);
                store.delete_session(session.id)?;
            }
            store.delete_file(file.id)?;
            report.files_removed += 1;
        }

        // Large files first so the long poles start early.
        jobs.sort_by_key(|job| std::cmp::Reverse(job.bytes()));
        let files_total = jobs.len();
        let cancel = AtomicBool::new(false);
        let (tx, rx) = std::sync::mpsc::sync_channel::<(Job, Result<FileParse>)>(8);
        let mut applier = Applier {
            store: &store,
            ft: &ft,
            names: ProjectNames::default(),
        };
        std::thread::scope(|scope| -> Result<()> {
            let cancel = &cancel;
            scope.spawn(move || {
                jobs.into_par_iter().for_each_with(tx, |tx, job| {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let parsed = parse_job(&job);
                    let _ = tx.send((job, parsed));
                });
            });
            let mut progress = Progress {
                files_total,
                bytes_total,
                ..Progress::default()
            };
            let mut since_checkpoint = 0;
            let outcome = (|| -> Result<()> {
                for (job, parsed) in rx.iter() {
                    progress.files_done += 1;
                    progress.bytes_done += job.bytes();
                    match parsed {
                        Ok(parsed) => {
                            report.files_parsed += 1;
                            report.bytes_parsed += job.bytes();
                            report.records_added += parsed
                                .sessions
                                .iter()
                                .map(|s| s.records.len() as u64)
                                .sum::<u64>();
                            report.bad_lines += parsed.bad_lines as u64;
                            since_checkpoint += job.bytes();
                            applier.apply(&writer, &job, parsed)?;
                        }
                        Err(err) => report
                            .failures
                            .push((job.found.path.display().to_string(), format!("{err:#}"))),
                    }
                    if let Some(report_progress) = opts.progress {
                        report_progress(progress);
                    }
                    if since_checkpoint > CHECKPOINT_BYTES {
                        writer.commit()?;
                        store.commit()?;
                        store.begin()?;
                        since_checkpoint = 0;
                    }
                }
                Ok(())
            })();
            if outcome.is_err() {
                cancel.store(true, Ordering::Relaxed);
                // Drain so the parsers can finish and the scope can end.
                for _ in rx.iter() {}
            }
            outcome
        })?;
        writer.commit()?;
        store.set_meta("refreshed_ms", &crate::timefmt::now_ms().to_string())?;
        store.commit()?;
        Ok(())
    })();
    if let Err(err) = result {
        store.rollback();
        let _ = writer.rollback();
        return Err(err);
    }
    writer.wait_merging_threads()?;
    ft.refresh_reader()?;
    if opts.in_query {
        report.merges_due = ft.merges_due()?;
    }
    report.took = started.elapsed();
    Ok(Some(report))
}

fn open_for_writing(paths: &IndexPaths, rebuild: bool, report: &mut Report) -> Result<Store> {
    if rebuild {
        wipe(paths)?;
        report.rebuilt = true;
    }
    let mut store = Store::open(&paths.db())?;
    let fulltext_ok = match &store {
        Some(store) => store.meta("fulltext")?.as_deref() == Some(&FULLTEXT_VERSION.to_string()),
        None => false,
    };
    if store.is_none() || (!fulltext_ok && paths.fulltext().exists()) {
        drop(store);
        wipe(paths)?;
        report.rebuilt = true;
        store = Store::open(&paths.db())?;
    }
    let store = store.context("index database has an unexpected schema after a rebuild")?;
    if !fulltext_ok || report.rebuilt {
        store.set_meta("fulltext", &FULLTEXT_VERSION.to_string())?;
    }
    Ok(store)
}

fn wipe(paths: &IndexPaths) -> Result<()> {
    for name in ["meta.sqlite", "meta.sqlite-wal", "meta.sqlite-shm"] {
        let path = paths.dir.join(name);
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    if paths.fulltext().exists() {
        std::fs::remove_dir_all(paths.fulltext())
            .with_context(|| format!("remove {}", paths.fulltext().display()))?;
    }
    Ok(())
}

/// What to do with a file seen before; `None` when it is unchanged.
fn plan_existing(f: &Found, row: &FileRow, store: &Store) -> Option<Plan> {
    if row.parser != f.kind.version() || row.inode != f.inode {
        return Some(Plan::Full);
    }
    if f.size == row.size && f.mtime_ns == row.mtime_ns {
        return None;
    }
    // Only changed files need their (sometimes large) parser state.
    let saved = store.file_state(row.id).ok().flatten();
    if !f.kind.is_jsonl() {
        return Some(Plan::Update {
            from: 0,
            state: saved.unwrap_or_default(),
        });
    }
    // Compressed files are written once; `parsed` counts decompressed bytes.
    if reader::is_compressed(&f.path) || f.size < row.parsed {
        return Some(Plan::Full);
    }
    let state: FileState = saved
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    // Rewritten in place (same inode, at least as long)? The bytes before the offset changed.
    if state.check != tail_check(&f.path, row.parsed) {
        return Some(Plan::Full);
    }
    Some(Plan::Update {
        from: row.parsed,
        state: saved.unwrap_or_default(),
    })
}

/// A fingerprint of the bytes just before `end`, to notice in-place rewrites.
fn tail_check(path: &Path, end: u64) -> Option<u64> {
    use sha2::Digest;
    if end == 0 {
        return None;
    }
    let file = File::open(path).ok()?;
    let start = end.saturating_sub(256);
    let mut buf = vec![0u8; (end - start) as usize];
    file.read_exact_at(&mut buf, start).ok()?;
    let digest = sha2::Sha256::digest(&buf);
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    Some(u64::from_be_bytes(bytes))
}

/// Decompressed size of a zstd file: from the frame header when recorded there (codex
/// writes it), else by decompressing.
fn zstd_content_size(path: &Path) -> Option<u64> {
    let file = File::open(path).ok()?;
    let mut header = [0u8; 18];
    let n = file.read_at(&mut header, 0).ok()?;
    if let Ok(Some(size)) = zstd::zstd_safe::get_frame_content_size(&header[..n]) {
        return Some(size);
    }
    let mut stream = reader::open_stream(path).ok()?;
    std::io::copy(&mut stream, &mut std::io::sink()).ok()
}

fn scan(roots: &Roots) -> Vec<Found> {
    source::discover(roots)
        .into_par_iter()
        .filter_map(|(kind, path)| {
            let meta = std::fs::metadata(&path).ok()?;
            let mut size = meta.len();
            let mut mtime_ns = meta.mtime() * 1_000_000_000 + meta.mtime_nsec();
            if kind == Kind::OpenCode
                && let Ok(wal) = std::fs::metadata(source::opencode::wal_path(&path))
            {
                // Commits land in the WAL first; the database file changes only at checkpoints.
                size += wal.len();
                mtime_ns = mtime_ns.max(wal.mtime() * 1_000_000_000 + wal.mtime_nsec());
            }
            if kind == Kind::Grok
                && let Ok(summary) = std::fs::metadata(source::grok::summary_path(&path))
            {
                // Grok writes the title and last active time to summary.json, often well
                // after the transcript; a change there alone re-reads it, parsing no lines.
                mtime_ns = mtime_ns.max(summary.mtime() * 1_000_000_000 + summary.mtime_nsec());
            }
            Some(Found {
                kind,
                size,
                mtime_ns,
                inode: meta.ino(),
                path,
            })
        })
        .collect()
}

fn parse_job(job: &Job) -> Result<FileParse> {
    let (from, state) = match &job.plan {
        Plan::Full => (0, None),
        Plan::Update { from, state } => (*from, Some(state.as_str())),
    };
    let mut parsed = source::parse_file(job.found.kind, &job.found.path, from, state)?;
    if job.found.kind.is_jsonl() && !reader::is_compressed(&job.found.path) {
        parsed.state["check"] = serde_json::to_value(tail_check(&job.found.path, parsed.end))?;
    }
    Ok(parsed)
}

struct Applier<'a> {
    store: &'a Store,
    ft: &'a FullText,
    names: ProjectNames,
}

impl Applier<'_> {
    fn apply(&mut self, writer: &tantivy::IndexWriter, job: &Job, parsed: FileParse) -> Result<()> {
        let f = &job.found;
        let kind = f.kind;
        let path = f.path.to_string_lossy().into_owned();
        let file_id = self.store.put_file(&FileRow {
            id: 0,
            path: path.clone(),
            source: kind.name().to_string(),
            size: f.size,
            mtime_ns: f.mtime_ns,
            inode: f.inode,
            parsed: parsed.end,
            parser: kind.version(),
            state: Some(parsed.state.to_string()),
        })?;
        let key_of = |sid: &str| source::session_key(kind, sid, &f.path);
        let keys: HashSet<i64> = parsed
            .sessions
            .iter()
            .map(|u| key_of(u.meta.session_id.as_deref().unwrap_or_default()))
            .collect();
        let mut drop = Vec::new();
        if parsed.full {
            for old in self.store.sessions_of_file(file_id)? {
                if !keys.contains(&old.id) {
                    drop.push(old.id);
                }
            }
        }
        drop.extend(parsed.gone.iter().map(|sid| key_of(sid)));
        for key in drop {
            self.ft.delete_session(writer, key);
            self.store.delete_session(key)?;
        }
        for update in &parsed.sessions {
            self.apply_session(writer, f, file_id, &path, update)?;
        }
        Ok(())
    }

    fn apply_session(
        &mut self,
        writer: &tantivy::IndexWriter,
        f: &Found,
        file_id: i64,
        path: &str,
        update: &source::SessionUpdate,
    ) -> Result<()> {
        let meta = &update.meta;
        let session_id = meta.session_id.clone().unwrap_or_default();
        let key = source::session_key(f.kind, &session_id, &f.path);
        self.ft.delete_from(writer, key, update.from_seq)?;
        self.store.delete_records_from(key, update.from_seq)?;
        let mut rows = Vec::with_capacity(update.records.len());
        for rec in &update.records {
            let cap = if rec.role.is_message() {
                INDEX_MESSAGE_BYTES
            } else {
                INDEX_TOOL_BYTES
            };
            let body = &rec.text[..rec.text.floor_char_boundary(cap)];
            writer.add_document(self.ft.doc(
                key,
                rec.seq,
                rec.role as u8,
                rec.ts,
                body,
                Some(body),
            ))?;
            rows.push(RecordRow {
                seq: rec.seq,
                pos: rec.pos,
                len: rec.len,
                part: rec.part,
                ts: rec.ts,
                role: rec.role as u8,
                tool: rec.tool.clone(),
                chars: rec.text.chars().count() as u64,
            });
        }
        self.store.put_records(key, &rows)?;
        if update.total == 0 {
            // Nothing to show yet (a header-only file); list it once it has content.
            self.store.delete_session(key)?;
            return Ok(());
        }
        let project = meta
            .cwd
            .as_deref()
            .map(|cwd| self.names.name(cwd))
            .unwrap_or_default();
        self.store.put_session(&SessionRow {
            id: key,
            file_id,
            source: f.kind.name().to_string(),
            session_id,
            path: path.to_string(),
            cwd: meta.cwd.clone(),
            project,
            title: meta.title.clone(),
            started_ms: meta.started_ms,
            last_ms: meta.last_ms.unwrap_or(f.mtime_ns / 1_000_000),
            messages: update.messages,
            records: update.total as u64,
            kind: meta.kind.clone(),
            parent: meta.parent.clone(),
            extra: (!meta.extra.is_empty())
                .then(|| serde_json::to_string(&meta.extra))
                .transpose()?,
        })?;
        Ok(())
    }
}

/// Exclusive, advisory lock held while writing; released when dropped.
struct WriterLock {
    _file: File,
}

impl WriterLock {
    fn acquire(path: &Path, wait: bool) -> Result<Option<WriterLock>> {
        use std::os::fd::AsRawFd;
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        let flags = if wait {
            libc::LOCK_EX
        } else {
            libc::LOCK_EX | libc::LOCK_NB
        };
        // SAFETY: flock on a descriptor we own.
        if unsafe { libc::flock(file.as_raw_fd(), flags) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(None);
            }
            return Err(err).with_context(|| format!("lock {}", path.display()));
        }
        Ok(Some(WriterLock { _file: file }))
    }
}

/// How many files and bytes a refresh would have to parse, without parsing anything.
pub fn pending(paths: &IndexPaths, roots: &Roots) -> Result<(usize, u64)> {
    let store = Store::open_readonly(&paths.db()).ok();
    let known: HashMap<String, FileRow> = store
        .as_ref()
        .and_then(|s| s.files().ok())
        .unwrap_or_default();
    let mut files = 0;
    let mut bytes = 0;
    for f in scan(roots) {
        let todo = match (known.get(f.path.to_str().unwrap_or_default()), &store) {
            (Some(row), Some(store)) => plan_existing(&f, row, store).map(|plan| match plan {
                Plan::Full => f.size,
                Plan::Update { from, .. } => f.size.saturating_sub(from),
            }),
            _ => Some(f.size),
        };
        if let Some(n) = todo {
            files += 1;
            bytes += n;
        }
    }
    Ok((files, bytes))
}

/// Start `sessift index` in the background, detached from this process and its terminal.
pub fn spawn_indexer(paths: &crate::config::Paths) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;
    crate::config::ensure_private_dir(&paths.log_dir())?;
    let log_path = paths.log_dir().join("index.log");
    if std::fs::metadata(&log_path).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = std::fs::rename(&log_path, log_path.with_extension("log.1"));
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let exe = std::env::current_exe().context("locate the sessift binary")?;
    let mut command = std::process::Command::new(exe);
    command
        .args(["index", "--quiet"])
        .env("SESSIFT_HOME", &paths.home)
        .current_dir("/")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: only async-signal-safe calls between fork and exec. The intermediate child
    // exits at once, so the indexer is never our child and leaves no zombie.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            match libc::fork() {
                -1 => Err(std::io::Error::last_os_error()),
                0 => Ok(()),
                _ => libc::_exit(0),
            }
        });
    }
    let mut child = command.spawn().context("start the background indexer")?;
    let _ = child.wait();
    Ok(())
}
