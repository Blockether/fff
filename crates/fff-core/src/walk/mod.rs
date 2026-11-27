//! Filesystem traversal backend. Selects one implementation at compile time:
//! - `zlob`: zlob's native parallel walker (requires the Zig toolchain).
//! - `ripgrep`: the `ignore` crate (ripgrep's walker), used by default.
//!
//! Both expose [`walk_collect_files`] with identical semantics so the rest of
//! the crate stays backend-agnostic.

use crate::types::FileItem;
use std::path::Path;

#[cfg(feature = "zlob")]
mod zlob;
#[cfg(feature = "zlob")]
pub(crate) use zlob::walk_collect_files;

#[cfg(all(not(feature = "zlob"), feature = "ripgrep"))]
mod ripgrep;
#[cfg(all(not(feature = "zlob"), feature = "ripgrep"))]
pub(crate) use ripgrep::walk_collect_files;

pub(crate) struct WalkOutput {
    pub(crate) pairs: Vec<(FileItem, String)>,
    /// Every non-root directory accepted by the walker, including empty ones.
    /// Paths are canonical, relative, and end in `/`.
    pub(crate) dirs: Vec<String>,
    pub(crate) ignore_rules: Option<WalkIgnoreRules>,
}

#[cfg_attr(not(feature = "zlob"), derive(Clone))]
pub(crate) struct WalkIgnoreRules {
    #[cfg(feature = "zlob")]
    inner: ::zlob::walk::WalkerOutcomeRules,
    #[cfg(feature = "zlob")]
    skip_hidden: bool,
    #[cfg(not(feature = "zlob"))]
    inner: ::ignore::IncrementalIgnore,
}

// SAFETY: the underlying storage is immutable, heap-owned, and thread-safe to
// read from concurrently (mirrors zlob's `IgnoreRules: Send + Sync`).
#[cfg(feature = "zlob")]
unsafe impl Send for WalkIgnoreRules {}
#[cfg(feature = "zlob")]
unsafe impl Sync for WalkIgnoreRules {}

impl std::fmt::Debug for WalkIgnoreRules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WalkIgnoreRules")
    }
}

impl WalkIgnoreRules {
    #[cfg(feature = "zlob")]
    pub(crate) fn is_ignored(&self, relative_path: &Path, _is_dir: bool) -> bool {
        if self.skip_hidden
            && relative_path
                .iter()
                .any(|name| name.as_encoded_bytes().starts_with(b"."))
        {
            return true;
        }
        self.inner
            .rules()
            .is_some_and(|rules| rules.is_ignored(relative_path))
    }

    #[cfg(not(feature = "zlob"))]
    pub(crate) fn is_ignored(&mut self, relative_path: &Path, is_dir: bool) -> bool {
        self.inner.matched(relative_path, is_dir).is_ignore()
    }
}

#[cfg(test)]
mod tests {
    use super::walk_collect_files;
    #[cfg(feature = "ripgrep")]
    use crate::ignore::CACHE_DIRS;
    use crate::ignore::IgnoreOverlay;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Backend-agnostic parity check: both the zlob and ripgrep walkers must
    // respect .gitignore, skip hidden files in a git repo, and surface the
    // expected file set with a correct synced count.
    #[test]
    fn collects_files_respecting_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::write(root.join("Cargo.toml"), "x").unwrap();
        fs::write(root.join("debug.log"), "").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("target/out.bin"), "bin").unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            root,
            Some(crate::scm::ScmBackend::configured()),
            false,
            true,
            &IgnoreOverlay::default().compile(root),
            1,
            &counter,
            &Default::default(),
            None,
        )
        .unwrap();

        let mut names: Vec<String> = out.pairs.into_iter().map(|(_, rel)| rel).collect();
        names.sort();
        let mut dirs = out.dirs;
        dirs.sort();

        assert!(names.contains(&"Cargo.toml".to_string()));
        assert!(names.iter().any(|n| n.ends_with("main.rs")));
        // target/ and *.log are gitignored; .git/ is skipped.
        assert!(!names.iter().any(|n| n.contains("target")));
        assert!(!names.iter().any(|n| n.ends_with(".log")));
        assert!(!names.iter().any(|n| n.contains(".git/")));
        assert!(dirs.contains(&"src/".to_string()));
        assert!(!dirs.iter().any(|dir| dir.contains("target/")));
        assert!(!dirs.iter().any(|dir| dir.contains(".git/")));
        assert_eq!(counter.load(Ordering::Relaxed), names.len());
    }

    #[test]
    fn parallel_walk_preserves_files_directories_and_count() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        for directory in 0..64 {
            let path = root.join(format!("src{directory}"));
            fs::create_dir(&path).unwrap();
            for file in 0..32 {
                fs::write(path.join(format!("file{file}.rs")), "fn main() {}\n").unwrap();
            }
        }
        let mut results = Vec::new();
        for threads in [1, 4] {
            let counter = Arc::new(AtomicUsize::new(0));
            let out = walk_collect_files(
                root,
                Some(crate::scm::ScmBackend::configured()),
                false,
                true,
                &IgnoreOverlay::default().compile(root),
                threads,
                &counter,
                &Default::default(),
                None,
            )
            .unwrap();
            let mut files: Vec<_> = out
                .pairs
                .into_iter()
                .map(|(item, path)| (path, item.size))
                .collect();
            files.sort();
            let mut directories = out.dirs;
            directories.sort();
            results.push((files, directories, counter.load(Ordering::Relaxed)));
        }
        assert_eq!(results[0], results[1]);
        assert_eq!(results[1].2, 64 * 32);
    }

    #[test]
    fn skips_git_metadata_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("module")).unwrap();
        fs::write(root.join(".git"), "gitdir: ../admin\n").unwrap();
        fs::write(
            root.join("module/.git"),
            "gitdir: ../../admin/modules/lib\n",
        )
        .unwrap();
        fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("module/lib.rs"), "pub fn lib() {}\n").unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            root,
            Some(crate::scm::ScmBackend::configured()),
            false,
            true,
            &IgnoreOverlay::default().compile(root),
            4,
            &counter,
            &Default::default(),
            None,
        )
        .unwrap();
        let mut paths: Vec<_> = out.pairs.into_iter().map(|(_, path)| path).collect();
        paths.sort();
        assert_eq!(
            (paths, counter.load(Ordering::Relaxed)),
            (vec!["main.rs".to_string(), "module/lib.rs".to_string()], 2)
        );
    }

    // Non-git roots prune known non-code directories (node_modules).
    #[test]
    fn prunes_non_code_dirs_for_non_git_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("node_modules")).unwrap();
        fs::write(root.join("node_modules/lib.js"), "x").unwrap();
        fs::write(root.join("index.js"), "x").unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            root,
            None,
            false,
            true,
            &IgnoreOverlay::default().compile(root),
            1,
            &counter,
            &Default::default(),
            None,
        )
        .unwrap();
        let names: Vec<String> = out.pairs.into_iter().map(|(_, rel)| rel).collect();

        assert!(names.iter().any(|n| n.ends_with("index.js")));
        assert!(!names.iter().any(|n| n.contains("node_modules")));
    }

    // Stow-style file symlinks pointing outside the root must be indexed with
    // follow_symlinks on, with the target's metadata (#912).
    #[cfg(unix)]
    #[test]
    fn follows_file_symlinks_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("dotfiles");
        let root = dir.path().join("config");
        fs::create_dir_all(outside.join("nested")).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(outside.join("sesh.toml"), "0123456789").unwrap();
        fs::write(outside.join("nested/inner.toml"), "x").unwrap();
        fs::write(root.join("projects.toml"), "x").unwrap();
        std::os::unix::fs::symlink("../dotfiles/sesh.toml", root.join("sesh.toml")).unwrap();
        std::os::unix::fs::symlink("../dotfiles/nested", root.join("linked")).unwrap();
        std::os::unix::fs::symlink("../dotfiles/missing", root.join("broken")).unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let overlay = IgnoreOverlay::default().compile(&root);
        let out = walk_collect_files(
            &root,
            None,
            true,
            true,
            &overlay,
            1,
            &counter,
            &Default::default(),
            None,
        )
        .unwrap();
        let mut names: Vec<String> = out.pairs.iter().map(|(_, rel)| rel.clone()).collect();
        names.sort();

        assert_eq!(names, ["linked/inner.toml", "projects.toml", "sesh.toml"]);
        assert_eq!(counter.load(Ordering::Relaxed), names.len());

        let (item, rel) = out.pairs.iter().find(|(_, r)| r == "sesh.toml").unwrap();
        assert_eq!(item.size, 10);
        assert_eq!(&rel[item.filename_offset_in_relative_path()..], "sesh.toml");

        let out = walk_collect_files(
            &root,
            None,
            false,
            true,
            &overlay,
            1,
            &counter,
            &Default::default(),
            None,
        )
        .unwrap();
        let names: Vec<String> = out.pairs.into_iter().map(|(_, rel)| rel).collect();
        assert_eq!(names, ["projects.toml"]);
    }

    // Every backend records the definition language while it walks. The ripgrep
    // walker once skipped it, so grep never classified its files as definitions.
    #[test]
    fn marks_definition_language_of_walked_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("notes.txt"), "main").unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let overlay = IgnoreOverlay::default().compile(root);
        let out = walk_collect_files(
            root,
            None,
            false,
            true,
            &overlay,
            1,
            &counter,
            &Default::default(),
            None,
        )
        .unwrap();
        let supports = |name: &str| {
            let (item, _) = out.pairs.iter().find(|(_, rel)| rel == name).unwrap();
            item.supports_definitions()
        };

        assert!(supports("main.rs"));
        assert!(!supports("notes.txt"));
    }

    // Only the zlob backend surfaces reusable ignore rules; they must match
    // the same tree the walk respected.
    #[cfg(feature = "zlob")]
    #[test]
    fn surfaces_reusable_ignore_rules() {
        use std::path::Path;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::write(root.join("Cargo.toml"), "x").unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            root,
            Some(crate::scm::ScmBackend::configured()),
            false,
            true,
            &IgnoreOverlay::default().compile(root),
            1,
            &counter,
            &Default::default(),
            None,
        )
        .unwrap();

        let rules = out.ignore_rules.expect("zlob surfaces ignore rules");
        assert!(rules.is_ignored(Path::new("target/"), true));
        assert!(rules.is_ignored(Path::new("debug.log"), false));
        assert!(!rules.is_ignored(Path::new("Cargo.toml"), false));
    }

    // Both backends must honor `info/exclude`, which linked worktrees read from
    // the common git dir (#982).
    #[test]
    fn honors_git_info_exclude() {
        use std::path::Path;
        use std::process::Command;

        let git = |dir: &Path, args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("excluded")).unwrap();
        fs::write(root.join("src/a.txt"), "a").unwrap();
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "init"]);
        fs::write(root.join("excluded/b.txt"), "b").unwrap();
        fs::write(root.join(".git/info/exclude"), "# comment\nexcluded/\n").unwrap();

        let worktree = dir.path().join("wt");
        git(
            &root,
            &["worktree", "add", "-q", worktree.to_str().unwrap()],
        );
        fs::create_dir_all(worktree.join("excluded")).unwrap();
        fs::write(worktree.join("excluded/c.txt"), "c").unwrap();

        for base in [&root, &worktree] {
            let counter = Arc::new(AtomicUsize::new(0));
            let out = walk_collect_files(
                base,
                Some(crate::scm::ScmBackend::configured()),
                false,
                true,
                &IgnoreOverlay::default().compile(base),
                1,
                &counter,
                &Default::default(),
                None,
            )
            .unwrap();
            let names: Vec<String> = out.pairs.iter().map(|(_, rel)| rel.clone()).collect();

            assert!(names.iter().any(|n| n == "src/a.txt"), "{names:?}");
            assert!(
                !names.iter().any(|n| n.starts_with("excluded")),
                "{names:?}"
            );

            // The watcher filters incremental updates with these rules.
            #[cfg(feature = "zlob")]
            {
                let rules = out.ignore_rules.expect("zlob surfaces ignore rules");
                assert!(rules.is_ignored(Path::new("excluded/later/new.txt"), false));
                assert!(rules.is_ignored(Path::new("debug.log"), false));
                assert!(!rules.is_ignored(Path::new("src/a.txt"), false));
            }
        }
    }

    // zlob 1.8 reads core.excludesFile below the root .gitignore, as git does (#874).
    #[cfg(feature = "zlob")]
    #[test]
    fn honors_git_excludes_file_with_git_precedence() {
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".worktrees/wt1")).unwrap();
        fs::write(root.join("src/hit.ts"), "x").unwrap();
        fs::write(root.join(".worktrees/wt1/hit.ts"), "x").unwrap();
        fs::write(root.join("drop.tmp"), "x").unwrap();
        fs::write(root.join("keep.tmp"), "x").unwrap();
        fs::write(root.join(".gitignore"), "!keep.tmp\n").unwrap();

        let excludes = dir.path().join("excludes");
        fs::write(&excludes, "# comment\n.worktrees/\n*.tmp\nexcludes\n").unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "core.excludesFile", excludes.to_str().unwrap()],
        ] {
            let status = Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .unwrap();
            assert!(status.success());
        }

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            root,
            Some(crate::scm::ScmBackend::configured()),
            false,
            true,
            &IgnoreOverlay::default().compile(root),
            1,
            &counter,
            &Default::default(),
            None,
        )
        .unwrap();
        let names: Vec<String> = out.pairs.iter().map(|(_, rel)| rel.clone()).collect();

        assert!(names.iter().any(|n| n == "src/hit.ts"), "{names:?}");
        assert!(names.iter().any(|n| n == "keep.tmp"), "{names:?}");
        assert!(!names.iter().any(|n| n == "drop.tmp"), "{names:?}");
        assert!(!names.iter().any(|n| n.contains(".worktrees")), "{names:?}");

        // The watcher filters incremental updates with these rules.
        let rules = out.ignore_rules.expect("zlob surfaces ignore rules");
        assert!(rules.is_ignored(std::path::Path::new(".worktrees/wt1/new.ts"), false));
        assert!(!rules.is_ignored(std::path::Path::new("src/hit.ts"), false));
    }

    // disable_git must also turn off the git exclude files, not only .gitignore.
    #[cfg(feature = "zlob")]
    #[test]
    fn disable_git_skips_every_git_ignore_source() {
        use crate::file_picker::FilePickerIgnoreOptions;
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        for name in ["from-gitignore", "from-exclude", "from-global", "from-dot"] {
            fs::write(root.join(name), "x").unwrap();
        }
        fs::write(root.join(".gitignore"), "from-gitignore\n").unwrap();
        fs::write(root.join(".ignore"), "from-dot\n").unwrap();
        let excludes = dir.path().join("excludes");
        fs::write(&excludes, "from-global\n").unwrap();

        for args in [
            vec!["init", "-q"],
            vec!["config", "core.excludesFile", excludes.to_str().unwrap()],
        ] {
            let status = Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .unwrap();
            assert!(status.success());
        }
        fs::write(root.join(".git/info/exclude"), "from-exclude\n").unwrap();

        let walk = |disable_git| -> Vec<String> {
            let options = FilePickerIgnoreOptions {
                disable_git,
                ..Default::default()
            };
            let counter = Arc::new(AtomicUsize::new(0));
            walk_collect_files(
                &root,
                Some(crate::scm::ScmBackend::configured()),
                false,
                true,
                &IgnoreOverlay::default().compile(&root),
                1,
                &counter,
                &options,
                None,
            )
            .unwrap()
            .pairs
            .into_iter()
            .map(|(_, rel)| rel)
            .collect()
        };

        let enabled = walk(false);
        for name in ["from-gitignore", "from-exclude", "from-global", "from-dot"] {
            assert!(!enabled.iter().any(|n| n == name), "{name}: {enabled:?}");
        }

        let disabled = walk(true);
        for name in ["from-gitignore", "from-exclude", "from-global"] {
            assert!(disabled.iter().any(|n| n == name), "{name}: {disabled:?}");
        }
        assert!(!disabled.iter().any(|n| n == "from-dot"), "{disabled:?}");
        assert!(
            !disabled.iter().any(|n| n.starts_with(".git/")),
            "{disabled:?}"
        );
    }

    #[test]
    fn collects_ignored_files_when_ignore_rules_are_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::write(root.join(".ignore"), "*.log\n").unwrap();
        fs::write(root.join("target/out.bin"), "bin").unwrap();
        fs::write(root.join("debug.log"), "log").unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            root,
            Some(crate::scm::ScmBackend::configured()),
            false,
            false,
            &IgnoreOverlay::default().compile(root),
            1,
            &counter,
            &Default::default(),
            None,
        )
        .unwrap();
        let names: Vec<String> = out.pairs.into_iter().map(|(_, rel)| rel).collect();

        assert!(names.iter().any(|n| n.ends_with("target/out.bin")));
        assert!(names.iter().any(|n| n.ends_with("debug.log")));
        assert!(!names.iter().any(|n| n.contains(".git/")));
    }

    // `.rgignore` is the one ignore filename ripgrep's `ignore` crate does not
    // register on its own; the overlay opts it in per walk.
    #[cfg(feature = "ripgrep")]
    #[test]
    fn honors_custom_ignore_filenames() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".rgignore"), "skipped/\n").unwrap();
        fs::create_dir(root.join("skipped")).unwrap();
        fs::write(root.join("skipped/a.txt"), "x").unwrap();
        fs::write(root.join("kept.txt"), "x").unwrap();

        let overlay = IgnoreOverlay {
            custom_ignore_filenames: vec![".rgignore".into()],
            ..Default::default()
        }
        .compile(root);

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            root,
            Some(crate::scm::ScmBackend::configured()),
            false,
            true,
            &overlay,
            1,
            &counter,
            &Default::default(),
            None,
        )
        .unwrap();
        let names: Vec<String> = out.pairs.into_iter().map(|(_, rel)| rel).collect();

        assert!(names.iter().any(|n| n.ends_with("kept.txt")));
        assert!(!names.iter().any(|n| n.contains("skipped")));
    }

    // `exclude_globs` drop files the ignore files kept; `unignore_globs`
    // resurrect a subtree a .gitignore dropped (walker never descends it).
    #[cfg(feature = "ripgrep")]
    #[test]
    fn applies_exclude_and_unignore_globs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir_all(root.join("target/keep")).unwrap();
        fs::create_dir_all(root.join("target/skip")).unwrap();
        fs::create_dir(root.join("secrets")).unwrap();
        fs::write(root.join("target/keep/a.txt"), "x").unwrap();
        fs::write(root.join("target/skip/b.txt"), "x").unwrap();
        fs::write(root.join("secrets/key.pem"), "x").unwrap();
        fs::write(root.join("main.rs"), "x").unwrap();

        let overlay = IgnoreOverlay {
            exclude_globs: vec!["secrets/**".into()],
            unignore_globs: vec!["target/keep/**".into()],
            ..Default::default()
        }
        .compile(root);

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            root,
            Some(crate::scm::ScmBackend::configured()),
            false,
            true,
            &overlay,
            1,
            &counter,
            &Default::default(),
            None,
        )
        .unwrap();
        let names: Vec<String> = out.pairs.into_iter().map(|(_, rel)| rel).collect();

        assert!(names.iter().any(|n| n.ends_with("main.rs")));
        assert!(names.iter().any(|n| n.ends_with("target/keep/a.txt")));
        assert!(!names.iter().any(|n| n.contains("target/skip")));
        assert!(!names.iter().any(|n| n.contains("secrets")));
        assert_eq!(counter.load(Ordering::Relaxed), names.len());
    }

    #[cfg(feature = "ripgrep")]
    #[test]
    fn unignore_globs_do_not_reopen_cache_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("src")).unwrap();

        let mut ignore_lines = String::new();
        let mut unignore_globs = Vec::new();
        for cache_dir in CACHE_DIRS {
            fs::create_dir_all(root.join(cache_dir)).unwrap();
            fs::create_dir_all(root.join("src").join(cache_dir)).unwrap();
            fs::write(root.join(cache_dir).join("root.cache"), "cache").unwrap();
            fs::write(
                root.join("src").join(cache_dir).join("nested.cache"),
                "cache",
            )
            .unwrap();
            ignore_lines.push_str(&format!("{cache_dir}/\nsrc/{cache_dir}/\n"));
            unignore_globs.push(format!("{cache_dir}/**"));
            unignore_globs.push(format!("src/{cache_dir}/**"));
        }
        fs::write(root.join(".gitignore"), ignore_lines).unwrap();
        fs::write(root.join("src/main.clj"), "source").unwrap();

        let overlay = IgnoreOverlay {
            unignore_globs,
            ..Default::default()
        }
        .compile(root);

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            root,
            Some(crate::scm::ScmBackend::configured()),
            false,
            true,
            &overlay,
            1,
            &counter,
            &Default::default(),
            None,
        )
        .unwrap();
        let names: Vec<String> = out.pairs.into_iter().map(|(_, rel)| rel).collect();
        let has_cache_component = |path: &str| {
            std::path::Path::new(path).components().any(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .is_some_and(|name| CACHE_DIRS.contains(&name))
            })
        };

        assert!(names.iter().any(|name| name == "src/main.clj"));
        assert!(
            !names.iter().any(|name| has_cache_component(name)),
            "cache files escaped pruning: {names:?}"
        );
        assert!(
            !out.dirs.iter().any(|dir| has_cache_component(dir)),
            "cache directories escaped pruning: {:?}",
            out.dirs
        );
        assert_eq!(counter.load(Ordering::Relaxed), names.len());
    }

    #[cfg(feature = "ripgrep")]
    #[test]
    fn unignore_globs_cannot_walk_above_base_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let base = root.join("project");
        let outside = root.join("outside");
        fs::create_dir_all(base.join(".git")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(base.join("main.rs"), "source").unwrap();
        fs::write(outside.join("secret.txt"), "outside").unwrap();

        let overlay = IgnoreOverlay {
            unignore_globs: vec!["../outside/**".into()],
            ..Default::default()
        }
        .compile(&base);

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            &base,
            Some(crate::scm::ScmBackend::configured()),
            false,
            true,
            &overlay,
            1,
            &counter,
            &Default::default(),
            None,
        )
        .unwrap();
        let names: Vec<String> = out.pairs.into_iter().map(|(_, rel)| rel).collect();

        assert!(names.iter().any(|name| name == "main.rs"));
        assert!(
            names.iter().all(|name| !name.starts_with("../")),
            "unignore walked outside the base path: {names:?}"
        );
        assert_eq!(counter.load(Ordering::Relaxed), names.len());
    }
    #[cfg(all(feature = "ripgrep", not(feature = "zlob")))]
    #[test]
    fn subtree_walk_preserves_project_overlay_and_stays_in_subtree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        for path in ["new/ignored", "new/empty/deep", "other"] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        fs::write(root.join(".rgignore"), "custom.txt\n").unwrap();
        for path in [
            "new/keep.txt",
            "new/drop.txt",
            "new/custom.txt",
            "new/ignored/keep.txt",
            "new/ignored/drop.txt",
            "other/keep.txt",
        ] {
            fs::write(root.join(path), "source").unwrap();
        }
        let overlay = IgnoreOverlay {
            custom_ignore_filenames: vec![".rgignore".into()],
            exclude_globs: vec!["/new/drop.txt".into(), "/new/ignored/drop.txt".into()],
            unignore_globs: vec![
                "new/ignored/**".into(),
                "other/**".into(),
                "**/keep.txt".into(),
            ],
        }
        .compile(root);
        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            &root.join("new"),
            Some(crate::scm::ScmBackend::configured()),
            false,
            true,
            &overlay,
            1,
            &counter,
            &Default::default(),
            None,
        )
        .unwrap();
        let mut names: Vec<_> = out.pairs.into_iter().map(|(_, rel)| rel).collect();
        names.sort();
        assert_eq!(names, ["ignored/keep.txt", "keep.txt"]);
        assert!(out.dirs.iter().any(|dir| dir == "empty/deep/"));
    }

    // A prefix-free glob walks the base again: it must add only what the
    // ignore files dropped, once, even when another root overlaps it.
    #[cfg(feature = "ripgrep")]
    #[test]
    fn prefix_free_unignore_globs_add_only_ignored_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "build/\n*.log\n").unwrap();
        for path in ["src/.vis", "build/.vis/skills", ".vis"] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        for path in [
            "src/main.rs",
            "src/.vis/notes.md",
            "src/debug.log",
            "build/out.bin",
            "build/.vis/skills/SKILL.md",
            ".vis/run.log",
        ] {
            fs::write(root.join(path), "x").unwrap();
        }

        let overlay = IgnoreOverlay {
            unignore_globs: vec!["**/.vis/**".into(), "build/.vis/**".into()],
            ..Default::default()
        }
        .compile(root);

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            root,
            Some(crate::scm::ScmBackend::configured()),
            false,
            true,
            &overlay,
            2,
            &counter,
            &Default::default(),
            None,
        )
        .unwrap();
        let mut names: Vec<String> = out.pairs.into_iter().map(|(_, rel)| rel).collect();
        names.sort();

        assert_eq!(
            names,
            [
                ".gitignore",
                ".vis/run.log",
                "build/.vis/skills/SKILL.md",
                "src/.vis/notes.md",
                "src/main.rs",
            ]
        );
        assert_eq!(counter.load(Ordering::Relaxed), names.len());
        assert!(out.dirs.iter().any(|dir| dir == "build/.vis/skills/"));
        assert!(!out.dirs.iter().any(|dir| dir == "build/"));
    }
}
