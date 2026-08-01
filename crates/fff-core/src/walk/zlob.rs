//! Filesystem traversal backed by zlob's native parallel walker.
//! Active when the `zlob` feature is enabled (requires the Zig toolchain).

use crate::file_picker::is_known_binary_extension_basename;
use crate::ignore::{CACHE_DIRS, IGNORED_DIRS};
use crate::types::FileItem;
use crate::walk::{WalkIgnoreRules, WalkOutput};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use zlob::walk::{WalkBuilder, WalkFlags, WalkMetadata, WalkState};

const PROGRESS_STEP: usize = 13;

#[tracing::instrument(skip_all, name = "zlob walker", level = "info")]
pub(crate) fn walk_collect_files(
    base_path: &Path,
    is_git_repo: bool,
    follow_symlinks: bool,
    respect_ignore_files: bool,
    overlay: &crate::ignore::CompiledIgnoreOverlay,
    threads: usize,
    synced_files_count: &Arc<AtomicUsize>,
) -> crate::Result<WalkOutput> {
    // zlob brings its own globber and ignore rules; the overlay is a ripgrep
    // backend feature, so surface the gap loudly instead of dropping it silently.
    if !overlay.is_empty() {
        tracing::warn!("ignore overlay is unsupported by the zlob walker; ignoring it");
    }
    // Skip hidden files on non-git roots and optionally follow symlinks.
    let mut flags = if respect_ignore_files {
        WalkFlags::GITIGNORE
    } else {
        WalkFlags::empty()
    };
    if !is_git_repo {
        flags |= WalkFlags::SKIP_HIDDEN;
    }
    if follow_symlinks {
        flags |= WalkFlags::FOLLOW_SYMLINKS;
    }

    let mut builder = WalkBuilder::new(base_path)
        .map_err(|e| crate::Error::WalkFailed(format!("WalkBuilder::new: {e:?}")))?;
    builder
        .options(flags)
        .threads(threads)
        // Bulk-fetch the only metadata FileItem needs; zlob never stats more.
        .metadata(WalkMetadata::SIZE | WalkMetadata::MTIME);

    // Cache trees are pruned unconditionally; non-git roots additionally prune
    // the known non-code directories.
    let extra: Vec<&str> = if is_git_repo {
        CACHE_DIRS.to_vec()
    } else {
        CACHE_DIRS
            .iter()
            .chain(IGNORED_DIRS.iter())
            .copied()
            .collect()
    };
    if !extra.is_empty()
        && let Err(e) = builder.extra_ignore(&extra)
    {
        // Interior NUL in one of the extra_ignore patterns would fail
        // here — treat as if no extras were supplied rather than
        // aborting the whole walk.
        tracing::warn!(?e, "zlob extra_ignore rejected; walking without it");
    }

    let pairs = parking_lot::Mutex::new(Vec::<(FileItem, String)>::new());
    let dirs = parking_lot::Mutex::new(Vec::<String>::new());

    let outcome = match builder.run(|entry| {
        if !entry.is_file() && !entry.is_dir() {
            return WalkState::Continue;
        }
        let rel_bytes = entry.relative_path_bytes();
        // zlob emits '/'-separated relative paths, which is fff's canonical
        // internal form on every platform — store them verbatim.
        let rel_str = String::from_utf8_lossy(rel_bytes).into_owned();

        if entry.is_dir() {
            // The walk root is implicit and has no searchable relative path.
            if !rel_str.is_empty() && rel_str != "." {
                let dir = format!("{}/", rel_str.trim_end_matches('/'));
                dirs.lock().push(dir);
            }
            return WalkState::Continue;
        }

        // `basename()` returns `&str` for files only.
        let basename = entry.basename().unwrap_or("");
        let is_binary = is_known_binary_extension_basename(basename);

        let size = entry.size().unwrap_or(0);
        // zlob reports mtime in ns since the Unix epoch; FileItem wants secs.
        let modified = entry
            .modified_ns()
            .map(|ns| (ns / 1_000_000_000).max(0) as u64)
            .unwrap_or(0);

        let basename_offset = entry.basename_offset_in_relative();
        let item = FileItem::new_raw(basename_offset, size, modified, None, is_binary);

        let mut guard = pairs.lock();
        guard.push((item, rel_str));
        let n = guard.len();
        drop(guard);

        if n % PROGRESS_STEP == 0 {
            synced_files_count.store(n, Ordering::Relaxed);
        }

        WalkState::Continue
    }) {
        Ok(outcome) => outcome,
        Err(e) => {
            // Preserve whatever we collected before the failure so the caller
            // can still surface a partial index instead of nothing.
            tracing::error!(?e, "zlob walk failed");
            return Err(crate::Error::WalkFailed(format!("{e:?}")));
        }
    };

    let pairs = pairs.into_inner();
    let mut dirs = dirs.into_inner();
    dirs.sort_unstable();
    dirs.dedup();
    // Always report the exact final total regardless of the last step.
    synced_files_count.store(pairs.len(), Ordering::Relaxed);

    // Retain the ignore rules only when the walk actually gathered some
    // (git roots with .gitignore/.ignore). Otherwise callers fall back.
    let ignore_rules = outcome
        .rules()
        .is_some()
        .then(|| WalkIgnoreRules { inner: outcome });

    Ok(WalkOutput {
        pairs,
        dirs,
        ignore_rules,
    })
}
