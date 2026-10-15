use crate::ignore::{
    CompiledIgnoreOverlay, has_cache_dir_component, is_cache_dir_name, non_git_repo_overrides,
};
use crate::types::FileItem;
use crate::walk::WalkOutput;
use crate::watch::is_git_file;
use ignore::WalkBuilder;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tracing::instrument(skip_all, name = "ripgrep walker", level = "info")]
pub(crate) fn walk_collect_files(
    base_path: &Path,
    is_git_repo: bool,
    follow_symlinks: bool,
    respect_ignore_files: bool,
    overlay: &CompiledIgnoreOverlay,
    threads: usize,
    synced_files_count: &Arc<AtomicUsize>,
) -> crate::Result<WalkOutput> {
    let mut walk_builder = WalkBuilder::new(base_path);
    walk_builder
        // this is a very important guard for the user opening ~/ or other root non-git dir
        .hidden(!is_git_repo)
        .git_ignore(respect_ignore_files)
        .git_exclude(respect_ignore_files)
        .git_global(respect_ignore_files)
        .ignore(respect_ignore_files)
        .follow_links(follow_symlinks)
        .threads(threads);

    // Extra ignore-file names (`.rgignore`): the `ignore` crate registers
    // `.ignore`/`.gitignore` only, so anything else is opt-in per walk.
    if respect_ignore_files {
        for name in overlay.custom_ignore_filenames() {
            walk_builder.add_custom_ignore_filename(name);
        }
    }

    if !is_git_repo && let Some(overrides) = non_git_repo_overrides(base_path) {
        walk_builder.overrides(overrides);
    }

    // Machine-generated cache trees are pruned at the DIRECTORY level, so the
    // walker never descends them even with `respect_ignore_files: false`.
    walk_builder.filter_entry(|entry| {
        entry.depth() == 0
            || !entry.file_type().is_some_and(|ft| ft.is_dir())
            || !entry.file_name().to_str().is_some_and(is_cache_dir_name)
    });

    let pairs = parking_lot::Mutex::new(Vec::<(FileItem, String)>::new());
    let dirs = parking_lot::Mutex::new(Vec::<String>::new());
    collect(
        walk_builder,
        base_path,
        synced_files_count,
        &pairs,
        &dirs,
        |rel, is_dir| !overlay.is_excluded(&base_path.join(rel), is_dir),
    );

    let mut pairs = pairs.into_inner();
    let mut dirs = dirs.into_inner();
    collect_unignored(
        base_path,
        follow_symlinks,
        overlay,
        threads,
        synced_files_count,
        &mut pairs,
        &mut dirs,
    );
    dirs.sort_unstable();
    dirs.dedup();

    Ok(WalkOutput {
        pairs,
        dirs,
        ignore_rules: None,
    })
}

/// Second, ignore-FREE pass over each unignore pattern's static prefix.
///
/// A gitignored directory is pruned before its children are ever seen, so
/// re-including `target/keep/**` is impossible from inside the main walk — the
/// subtree has to be walked again with the ignore files off. Entries that the
/// main walk already holds are skipped before any glob match or `stat`, so a
/// prefix-free pattern such as `**/.vis/**` costs about one directory listing
/// of the tree, not a second full scan.
fn collect_unignored(
    base_path: &Path,
    follow_symlinks: bool,
    overlay: &CompiledIgnoreOverlay,
    threads: usize,
    synced_files_count: &Arc<AtomicUsize>,
    pairs: &mut Vec<(FileItem, String)>,
    dirs: &mut Vec<String>,
) {
    let roots = unignore_walk_roots(base_path, overlay);
    let Some((first, rest)) = roots.split_first() else {
        return;
    };

    let mut builder = WalkBuilder::new(first);
    for root in rest {
        builder.add(root);
    }
    builder
        // Re-included on purpose: hidden trees count too.
        .hidden(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .ignore(false)
        .follow_links(follow_symlinks)
        .threads(threads);
    builder.filter_entry(|entry| {
        entry.depth() == 0
            || !entry.file_type().is_some_and(|ft| ft.is_dir())
            || !entry.file_name().to_str().is_some_and(is_cache_dir_name)
    });

    let extra = parking_lot::Mutex::new(Vec::<(FileItem, String)>::new());
    let extra_dirs = parking_lot::Mutex::new(Vec::<String>::new());
    {
        let known_files: HashSet<&str> = pairs.iter().map(|(_, rel)| rel.as_str()).collect();
        let known_dirs: HashSet<&str> = dirs.iter().map(String::as_str).collect();
        let new_files = parking_lot::Mutex::new(HashSet::<String>::new());
        collect(
            builder,
            base_path,
            synced_files_count,
            &extra,
            &extra_dirs,
            |rel, is_dir| {
                let known = if is_dir {
                    known_dirs.contains(rel)
                } else {
                    known_files.contains(rel)
                };
                if known {
                    return false;
                }
                let absolute_path = base_path.join(rel);
                overlay.is_unignored(&absolute_path, is_dir)
                    && !overlay.is_excluded(&absolute_path, is_dir)
                    && (is_dir || new_files.lock().insert(rel.to_owned()))
            },
        );
    }

    pairs.extend(extra.into_inner());
    dirs.extend(extra_dirs.into_inner());
}

/// Existing unignore roots inside `base_path`, without cache trees and
/// without roots that another root already contains.
fn unignore_walk_roots<'a>(
    base_path: &'a Path,
    overlay: &'a CompiledIgnoreOverlay,
) -> Vec<&'a Path> {
    let mut roots: Vec<&Path> = overlay
        .unignore_roots()
        .iter()
        .filter_map(|root| {
            if root.starts_with(base_path) {
                Some(root.as_path())
            } else if base_path.starts_with(root) {
                Some(base_path)
            } else {
                None
            }
        })
        .filter(|root| {
            !root
                .strip_prefix(base_path)
                .is_ok_and(has_cache_dir_component)
                && root.exists()
        })
        .collect();
    roots.sort_unstable();
    roots.dedup();
    let mut outer: Vec<&Path> = Vec::with_capacity(roots.len());
    for root in roots {
        if !outer.iter().any(|kept| root.starts_with(kept)) {
            outer.push(root);
        }
    }
    outer
}

/// Run `builder` in parallel, pushing every file whose base-relative path
/// passes `keep` into `out`. `keep` runs before the `stat`, so rejected
/// files cost no metadata call.
fn collect(
    builder: WalkBuilder,
    base_path: &Path,
    synced_files_count: &Arc<AtomicUsize>,
    files: &parking_lot::Mutex<Vec<(FileItem, String)>>,
    dirs: &parking_lot::Mutex<Vec<String>>,
    keep: impl Fn(&str, bool) -> bool + Send + Sync,
) {
    let keep = &keep;
    builder.build_parallel().run(|| {
        let counter = Arc::clone(synced_files_count);
        let base_path = base_path.to_path_buf();

        Box::new(move |result| {
            let Ok(entry) = result else {
                return ignore::WalkState::Continue;
            };
            let Some(file_type) = entry.file_type() else {
                return ignore::WalkState::Continue;
            };
            let path = entry.path();

            if file_type.is_dir() {
                if entry.depth() > 0
                    && let Some(rel_path) = canonical_relative_dir(path, &base_path)
                    && keep(&rel_path, true)
                {
                    dirs.lock().push(rel_path);
                }
                return ignore::WalkState::Continue;
            }
            if !file_type.is_file() {
                return ignore::WalkState::Continue;
            }

            // Ignore walkers sometimes surface files inside `.git/`
            // when the base is itself a git repo — skip them.
            if is_git_file(path) {
                return ignore::WalkState::Continue;
            }

            let rel_path = relative_file_path(path, &base_path);
            if !keep(&rel_path, false) {
                return ignore::WalkState::Continue;
            }

            let metadata = entry.metadata().ok();
            let file_item = file_item(path, &rel_path, metadata.as_ref());
            files.lock().push((file_item, rel_path));
            counter.fetch_add(1, Ordering::Relaxed);
            ignore::WalkState::Continue
        })
    });
}

fn canonical_relative_dir(path: &Path, base_path: &Path) -> Option<String> {
    let relative = path.strip_prefix(base_path).ok()?;
    let relative =
        crate::path_utils::to_canonical_slashes(&relative.to_string_lossy()).into_owned();
    (!relative.is_empty()).then(|| format!("{relative}/"))
}

fn relative_file_path(path: &Path, base_path: &Path) -> String {
    let relative = pathdiff::diff_paths(path, base_path).unwrap_or_else(|| path.to_path_buf());
    crate::path_utils::to_canonical_slashes(&relative.to_string_lossy()).into_owned()
}

fn file_item(path: &Path, rel_path: &str, metadata: Option<&std::fs::Metadata>) -> FileItem {
    let (size, modified) = metadata.map_or((0, 0), |metadata| {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_secs());
        (metadata.len(), modified)
    });
    let filename_start = rel_path
        .rfind(std::path::is_separator)
        .map_or(0, |index| index + 1) as u16;
    FileItem::new_raw(
        filename_start,
        size,
        modified,
        None,
        crate::file_picker::is_known_binary_extension(path),
    )
}
