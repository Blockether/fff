use crate::ignore::is_cache_dir_name;
use ignore::{WalkBuilder, WalkState};
use std::path::Path;
use std::time::UNIX_EPOCH;

/// How much of a directory [`list_directory`] hands back.
#[derive(Debug, Clone)]
pub struct ListOptions {
    /// Levels below the listed directory; `1` is its immediate children.
    pub max_depth: usize,
    /// Include dot-entries.
    pub include_hidden: bool,
    /// Honor `.gitignore`/`.ignore` (and their parents) while listing.
    pub respect_ignore_files: bool,
    /// Descend through symlinked directories. They are always LISTED.
    pub follow_symlinks: bool,
}

impl Default for ListOptions {
    fn default() -> Self {
        Self {
            max_depth: 1,
            include_hidden: false,
            respect_ignore_files: true,
            follow_symlinks: false,
        }
    }
}

/// One entry below the listed directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntry {
    /// `/`-separated path relative to the listed directory.
    pub relative_path: String,
    /// Last path segment.
    pub name: String,
    /// `1` for an immediate child.
    pub depth: usize,
    /// True for a directory, including a symlink pointing at one.
    pub is_dir: bool,
    /// Bytes for files, `0` for directories or when unknown.
    pub size: u64,
    /// Modification time in whole seconds since the Unix epoch, `0` when unknown.
    pub modified: u64,
}

/// List one directory, bounded by depth and without indexing anything.
///
/// A [`FilePicker`](crate::FilePicker) refuses a filesystem root or a home
/// directory ([`Error::FilesystemRoot`](crate::Error::FilesystemRoot)) because
/// INDEXING `/` is never what a caller wants. Answering "what is in this
/// directory" is a different question, and it has an answer everywhere: this
/// walk keeps the ignore rules and the cache-directory pruning of the indexing
/// walker, but it is stateless, bounded, and works on `/` and `$HOME` too.
///
/// Entries are sorted by their relative path. Symlinked directories are listed
/// (`is_dir` follows the link's TARGET) but only descended when
/// [`ListOptions::follow_symlinks`] is set.
pub fn list_directory_with(
    path: &Path,
    options: &ListOptions,
    overlay: &crate::ignore::IgnoreOverlay,
) -> crate::Result<Vec<ListEntry>> {
    let metadata =
        std::fs::metadata(path).map_err(|_| crate::Error::InvalidPath(path.to_path_buf()))?;
    if !metadata.is_dir() {
        return Err(crate::Error::InvalidPath(path.to_path_buf()));
    }
    let compiled = overlay.compile(path);
    let mut entries = collect_listing(path, options, &compiled, None);
    #[cfg(feature = "ripgrep")]
    if options.respect_ignore_files && !compiled.unignore_roots().is_empty() {
        entries.extend(collect_listing(
            path,
            options,
            &compiled,
            Some(compiled.unignore_roots()),
        ));
    }
    entries.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));
    entries.dedup_by(|left, right| left.relative_path == right.relative_path);
    Ok(entries)
}

/// List one directory without a caller ignore overlay.
pub fn list_directory(path: &Path, options: &ListOptions) -> crate::Result<Vec<ListEntry>> {
    list_directory_with(path, options, &crate::ignore::IgnoreOverlay::default())
}

fn collect_listing(
    path: &Path,
    options: &ListOptions,
    overlay: &crate::ignore::CompiledIgnoreOverlay,
    unignore_roots: Option<&[std::path::PathBuf]>,
) -> Vec<ListEntry> {
    let respect = options.respect_ignore_files && unignore_roots.is_none();
    let mut builder = WalkBuilder::new(path);
    builder
        .max_depth(Some(options.max_depth.max(1)))
        .hidden(!options.include_hidden)
        .parents(respect)
        .git_ignore(respect)
        .git_exclude(respect)
        .git_global(respect)
        .ignore(respect)
        .follow_links(options.follow_symlinks);
    if respect {
        for name in &overlay.source().custom_ignore_filenames {
            builder.add_custom_ignore_filename(name);
        }
    }
    let collected = parking_lot::Mutex::new(Vec::<ListEntry>::new());
    builder.build_parallel().run(|| {
        let collected = &collected;
        Box::new(move |result| {
            let Ok(entry) = result else {
                return WalkState::Continue;
            };
            let depth = entry.depth();
            if depth == 0 {
                return WalkState::Continue;
            }
            let Ok(relative) = entry.path().strip_prefix(path) else {
                return WalkState::Continue;
            };
            let file_type = entry.file_type();
            // List symlinks by their target type without following them.
            let is_dir = file_type.is_some_and(|ft| ft.is_dir())
                || (file_type.is_some_and(|ft| ft.is_symlink())
                    && std::fs::metadata(entry.path()).is_ok_and(|md| md.is_dir()));
            if overlay.is_excluded(entry.path(), is_dir) {
                return if is_dir {
                    WalkState::Skip
                } else {
                    WalkState::Continue
                };
            }
            if let Some(roots) = unignore_roots {
                let related = roots
                    .iter()
                    .any(|root| entry.path().starts_with(root) || root.starts_with(entry.path()));
                if !related {
                    return if is_dir {
                        WalkState::Skip
                    } else {
                        WalkState::Continue
                    };
                }
                if !is_dir && !overlay.is_unignored(entry.path(), false) {
                    return WalkState::Continue;
                }
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let metadata = entry.metadata().ok();
            collected.lock().push(ListEntry {
                relative_path: crate::path_utils::to_canonical_slashes(&relative.to_string_lossy())
                    .into_owned(),
                depth,
                is_dir,
                size: if is_dir {
                    0
                } else {
                    metadata.as_ref().map_or(0, |md| md.len())
                },
                modified: metadata
                    .as_ref()
                    .and_then(|md| md.modified().ok())
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |since| since.as_secs()),
                name: name.clone(),
            });
            // List cache trees, but never open them.
            if is_dir && is_cache_dir_name(&name) {
                WalkState::Skip
            } else {
                WalkState::Continue
            }
        })
    });
    collected.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn names(entries: &[ListEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.relative_path.as_str()).collect()
    }

    #[cfg(feature = "ripgrep")]
    #[test]
    fn overlay_listing_is_bounded_and_preserves_ignore_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("vendor/keep/deep")).unwrap();
        fs::write(root.join(".ignore"), "vendor/\n").unwrap();
        fs::write(root.join(".rgignore"), "hidden.txt\n").unwrap();
        for name in [
            "visible.txt",
            "hidden.txt",
            "secret.txt",
            "vendor/keep/a.txt",
            "vendor/keep/secret.txt",
            "vendor/keep/deep/too-deep.txt",
        ] {
            fs::write(root.join(name), "x").unwrap();
        }
        let overlay = crate::ignore::IgnoreOverlay {
            custom_ignore_filenames: vec![".rgignore".into()],
            exclude_globs: vec!["secret.txt".into()],
            unignore_globs: vec!["vendor/keep/**".into()],
        };
        let options = ListOptions {
            max_depth: 3,
            ..ListOptions::default()
        };
        let entries = list_directory_with(root, &options, &overlay).unwrap();
        assert_eq!(
            names(&entries),
            [
                "vendor",
                "vendor/keep",
                "vendor/keep/a.txt",
                "vendor/keep/deep",
                "visible.txt"
            ]
        );
        assert!(entries.iter().all(|entry| entry.depth <= 3));
        let shallow = list_directory_with(root, &ListOptions::default(), &overlay).unwrap();
        assert_eq!(names(&shallow), ["vendor", "visible.txt"]);
    }

    #[cfg(feature = "ripgrep")]
    #[test]
    fn overlays_keep_hidden_symlink_and_cache_guards() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("vendor/.git/objects")).unwrap();
        fs::create_dir_all(root.join("outside")).unwrap();
        fs::write(root.join(".ignore"), "vendor/\n").unwrap();
        fs::write(root.join(".rgignore"), "outside/\n").unwrap();
        for name in [
            "vendor/a.txt",
            "vendor/.hidden",
            "vendor/.git/objects/blob",
            "vendor/secret.txt",
            "outside/a.txt",
        ] {
            fs::write(root.join(name), "x").unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("outside"), root.join("vendor/link")).unwrap();
        let overlay = crate::ignore::IgnoreOverlay {
            custom_ignore_filenames: vec![".rgignore".into()],
            exclude_globs: vec!["secret.txt".into()],
            unignore_globs: vec!["vendor/**".into()],
        };
        let options = ListOptions {
            max_depth: 5,
            ..ListOptions::default()
        };
        let entries = list_directory_with(root, &options, &overlay).unwrap();
        assert!(names(&entries).contains(&"vendor/a.txt"));
        assert!(!names(&entries).iter().any(|p| p.contains(".hidden")
            || p.contains("objects")
            || p.contains("secret")
            || p.contains("link/")
            || p.starts_with("outside")));
        let options = ListOptions {
            respect_ignore_files: false,
            ..options
        };
        let entries = list_directory_with(root, &options, &overlay).unwrap();
        assert!(names(&entries).contains(&"outside/a.txt"));
        assert!(!names(&entries).contains(&"vendor/secret.txt"));
    }

    #[test]
    fn lists_immediate_children_and_hides_dot_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("visible.txt"), "hello").unwrap();
        fs::write(root.join(".hidden"), "x").unwrap();
        fs::create_dir(root.join("nested")).unwrap();
        fs::write(root.join("nested/deep.txt"), "deep").unwrap();

        let entries = list_directory(root, &ListOptions::default()).unwrap();
        assert_eq!(names(&entries), ["nested", "visible.txt"]);

        let file = entries.iter().find(|e| e.name == "visible.txt").unwrap();
        assert!(!file.is_dir);
        assert_eq!(file.size, 5);
        assert!(file.modified > 0);
        assert!(entries.iter().find(|e| e.name == "nested").unwrap().is_dir);

        let hidden = list_directory(
            root,
            &ListOptions {
                include_hidden: true,
                ..ListOptions::default()
            },
        )
        .unwrap();
        assert_eq!(names(&hidden), [".hidden", "nested", "visible.txt"]);
    }

    #[test]
    fn descends_to_the_requested_depth() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/deep.txt"), "deep").unwrap();

        let entries = list_directory(
            root,
            &ListOptions {
                max_depth: 2,
                ..ListOptions::default()
            },
        )
        .unwrap();
        assert_eq!(names(&entries), ["a", "a/b"]);
        assert_eq!(entries[1].depth, 2);
    }

    #[test]
    fn honors_ignore_files_unless_asked_not_to() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".ignore"), "ignored.txt\n").unwrap();
        fs::write(root.join("ignored.txt"), "x").unwrap();
        fs::write(root.join("kept.txt"), "x").unwrap();

        let entries = list_directory(root, &ListOptions::default()).unwrap();
        assert_eq!(names(&entries), ["kept.txt"]);

        let entries = list_directory(
            root,
            &ListOptions {
                respect_ignore_files: false,
                ..ListOptions::default()
            },
        )
        .unwrap();
        assert_eq!(names(&entries), ["ignored.txt", "kept.txt"]);
    }

    #[test]
    fn lists_a_symlinked_directory_without_descending_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("real")).unwrap();
        fs::write(root.join("real/inside.txt"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();

        #[cfg(unix)]
        {
            let entries = list_directory(
                root,
                &ListOptions {
                    max_depth: 2,
                    ..ListOptions::default()
                },
            )
            .unwrap();
            assert_eq!(names(&entries), ["link", "real", "real/inside.txt"]);
            assert!(entries.iter().find(|e| e.name == "link").unwrap().is_dir);
        }
    }

    #[test]
    fn does_not_open_cache_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::write(root.join(".git/objects/blob"), "x").unwrap();

        let entries = list_directory(
            root,
            &ListOptions {
                max_depth: 3,
                include_hidden: true,
                ..ListOptions::default()
            },
        )
        .unwrap();
        assert_eq!(names(&entries), [".git"]);
    }

    // The whole reason this module exists: a `FilePicker` refuses to index a
    // filesystem root or a home directory, which left every caller unable to
    // answer "what is in `/`" at all.
    #[test]
    fn lists_a_filesystem_root_and_a_home_directory() {
        let root = list_directory(Path::new("/"), &ListOptions::default()).unwrap();
        assert!(!root.is_empty(), "expected entries in the filesystem root");

        let home = dirs::home_dir().unwrap();
        let entries = list_directory(&home, &ListOptions::default()).unwrap();
        assert!(
            !entries.is_empty(),
            "expected entries in the home directory"
        );
    }

    #[test]
    fn refuses_a_file_and_a_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain.txt");
        std::fs::write(&file, "x").unwrap();

        assert!(matches!(
            list_directory(&file, &ListOptions::default()),
            Err(crate::Error::InvalidPath(_))
        ));
        assert!(matches!(
            list_directory(&dir.path().join("nope"), &ListOptions::default()),
            Err(crate::Error::InvalidPath(_))
        ));
    }
}
