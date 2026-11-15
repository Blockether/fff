/// Largest file whose full content fff will touch: the default grep read cap
/// (`GrepSearchOptions::max_file_size`) and the content size cap
/// (`ContentCacheBudget::max_file_size`). Binary detection also streams up to
/// this far so nothing grep would read is left unclassified.
pub const MAX_FFFILE_SIZE: u64 = 10 * 1024 * 1024;

/// Upper bound on a file the bigram builder will build, if the file is very large there is a
/// big probability it will only bloat the available bigrams and will anyway pop ut from the prefilter
pub const MAX_INDEXABLE_FILE_SIZE: usize = 2 * 1024 * 1024;

/// Default byte cap of [`crate::ContentCacheBudget`] for a small repo.
pub const MAX_CACHED_CONTENT_BYTES: u64 = 512 * 1024 * 1024;

/// Watcher overflow capacity reserved after the initial scan
pub const MAX_OVERFLOW_FILES: usize = 1024;

/// Minimum delay between watcher-initiated rescans.
pub const RESCAN_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Rescan delay for large indexes.
pub const RESCAN_MIN_INTERVAL_LARGE_INDEX: std::time::Duration =
    std::time::Duration::from_secs(5 * 60);

/// Live-file count at which [`RESCAN_MIN_INTERVAL_LARGE_INDEX`] takes over.
pub const LARGE_INDEX_FILE_COUNT: usize = 1_000_000;

// we do not support 32kb path limit on windows
#[cfg(target_os = "windows")]
pub const PATH_BUF_SIZE: usize = 4096;

#[cfg(not(target_os = "windows"))]
pub const PATH_BUF_SIZE: usize = libc::PATH_MAX as usize;
