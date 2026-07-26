use crate::ignore::{CompiledIgnoreOverlay, is_cache_dir_name, non_git_repo_overrides};
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
    collect(walk_builder, base_path, synced_files_count, &pairs, |rel| {
        !overlay.is_excluded(Path::new(rel), false)
    });

    let mut pairs = pairs.into_inner();
    collect_unignored(
        base_path,
        follow_symlinks,
        overlay,
        threads,
        synced_files_count,
        &mut pairs,
    );

    Ok(WalkOutput {
        pairs,
        ignore_rules: None,
    })
}

/// Second, ignore-FREE pass over each unignore pattern's static prefix.
///
/// A gitignored directory is pruned before its children are ever seen, so
/// re-including `target/keep/**` is impossible from inside the main walk — the
/// subtree has to be walked again with the ignore files off.
fn collect_unignored(
    base_path: &Path,
    follow_symlinks: bool,
    overlay: &CompiledIgnoreOverlay,
    threads: usize,
    synced_files_count: &Arc<AtomicUsize>,
    pairs: &mut Vec<(FileItem, String)>,
) {
    let roots = overlay.unignore_roots();
    if roots.is_empty() {
        return;
    }

    let already: HashSet<String> = pairs.iter().map(|(_, rel)| rel.clone()).collect();
    let extra = parking_lot::Mutex::new(Vec::<(FileItem, String)>::new());
    let mut seen_roots: HashSet<&Path> = HashSet::new();

    for root in roots {
        if !seen_roots.insert(root.as_path()) || !root.exists() {
            continue;
        }

        let mut builder = WalkBuilder::new(root);
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

        collect(builder, base_path, synced_files_count, &extra, |rel| {
            let rel_path = Path::new(rel);
            overlay.is_unignored(rel_path, false)
                && !overlay.is_excluded(rel_path, false)
                && !already.contains(rel)
        });
    }

    pairs.extend(extra.into_inner());
}

/// Run `builder` in parallel, pushing every file whose base-relative path
/// passes `keep` into `out`.
fn collect(
    builder: WalkBuilder,
    base_path: &Path,
    synced_files_count: &Arc<AtomicUsize>,
    out: &parking_lot::Mutex<Vec<(FileItem, String)>>,
    keep: impl Fn(&str) -> bool + Send + Sync,
) {
    let keep = &keep;
    builder.build_parallel().run(|| {
        let counter = Arc::clone(synced_files_count);
        let base_path = base_path.to_path_buf();

        Box::new(move |result| {
            let Ok(entry) = result else {
                return ignore::WalkState::Continue;
            };

            if entry.file_type().is_some_and(|ft| ft.is_file()) {
                let path = entry.path();

                // Ignore walkers sometimes surface files inside `.git/`
                // when the base is itself a git repo — skip them.
                if is_git_file(path) {
                    return ignore::WalkState::Continue;
                }

                let metadata = entry.metadata().ok();
                let (file_item, rel_path) =
                    FileItem::new_from_walk(path, &base_path, None, metadata.as_ref());

                if !keep(&rel_path) {
                    return ignore::WalkState::Continue;
                }

                out.lock().push((file_item, rel_path));
                counter.fetch_add(1, Ordering::Relaxed);
            }
            ignore::WalkState::Continue
        })
    });
}
