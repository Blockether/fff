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
pub fn list_directory(path: &Path, options: &ListOptions) -> crate::Result<Vec<ListEntry>> {
    let metadata =
        std::fs::metadata(path).map_err(|_| crate::Error::InvalidPath(path.to_path_buf()))?;
    if !metadata.is_dir() {
        return Err(crate::Error::InvalidPath(path.to_path_buf()));
    }

    let respect = options.respect_ignore_files;
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

    let root = path.to_path_buf();
    let collected = parking_lot::Mutex::new(Vec::<ListEntry>::new());
    builder.build_parallel().run(|| {
        let root = root.clone();
        let collected = &collected;

        Box::new(move |result| {
            let Ok(entry) = result else {
                return WalkState::Continue;
            };
            let depth = entry.depth();
            if depth == 0 {
                return WalkState::Continue;
            }
            let Ok(relative) = entry.path().strip_prefix(&root) else {
                return WalkState::Continue;
            };

            let file_type = entry.file_type();
            // A symlink is listed as what it POINTS AT, so a linked directory
            // reads as a directory without the walker ever descending it.
            let is_dir = file_type.is_some_and(|ft| ft.is_dir())
                || (file_type.is_some_and(|ft| ft.is_symlink())
                    && std::fs::metadata(entry.path()).is_ok_and(|md| md.is_dir()));
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

            // Cache trees are listed but never opened: nobody wants the 52k
            // blobs under `.git/` or `.cache/` counted one by one.
            if is_dir && is_cache_dir_name(&name) {
                WalkState::Skip
            } else {
                WalkState::Continue
            }
        })
    });

    let mut entries = collected.into_inner();
    entries.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn names(entries: &[ListEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.relative_path.as_str()).collect()
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
