use std::path::Path;

/// Directories excluded when walking a non-git root. Entries are `cfg`-gated
/// so a single iteration covers standard + platform-specific overrides.
pub(crate) const IGNORED_DIRS: &[&str] = &[
    "node_modules",
    "__pycache__",
    "venv",
    ".venv",
    // Rust (glob-only patterns for non_git_repo_overrides; is_non_code_directory
    // matches the "target" component separately).
    "target/debug",
    "target/release",
    "target/rust-analyzer",
    "target/criterion",
    #[cfg(target_os = "macos")]
    "Library/Application Support",
    #[cfg(target_os = "macos")]
    "Library/Caches",
    // App-group sandbox storage — used by iMessage, Photos, Notes, Calendar,
    // Electron apps, etc. for SQLite-WAL, LevelDB, protobuf files. These are
    // almost entirely extension-less binary files (~80k on a typical $HOME)
    // that never need to appear in a fuzzy or grep search.
    #[cfg(target_os = "macos")]
    "Library/Group Containers",
    #[cfg(target_os = "macos")]
    "Library/Containers",
    #[cfg(target_os = "windows")]
    "bin/Debug",
    #[cfg(target_os = "windows")]
    "bin/Release",
    #[cfg(target_os = "windows")]
    "Program Files",
    #[cfg(target_os = "windows")]
    "Program Files (x86)",
    #[cfg(target_os = "windows")]
    "AppData/Local",
    #[cfg(target_os = "windows")]
    "AppData/Roaming",
];

/// Directory names that hold ONLY machine-generated cache/VCS bytes: never
/// human-authored, never worth grepping. Pruned on every walk — git or not,
/// ignore-files respected or not — because `respect_ignore_files: false` means
/// "show me the ignored SOURCE", not "walk 52k clj-kondo cache blobs".
pub(crate) const CACHE_DIRS: &[&str] = &[
    ".git",
    ".cache", // clj-kondo, clojure-lsp, yarn, …
    ".cpcache",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".parcel-cache",
    ".sass-cache",
    ".turbo",
    ".gradle",
];

/// True when `name` is a directory component that only ever holds cache bytes.
pub(crate) fn is_cache_dir_name(name: &str) -> bool {
    CACHE_DIRS.contains(&name)
}

/// True when a base-relative path enters an unconditionally pruned cache tree.
pub(crate) fn has_cache_dir_component(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(is_cache_dir_name)
    })
}

#[cfg(all(not(feature = "zlob"), feature = "ripgrep"))]
pub(crate) fn non_git_repo_overrides(base_path: &Path) -> Option<ignore::overrides::Override> {
    use ignore::overrides::OverrideBuilder;

    let mut builder = OverrideBuilder::new(base_path);
    for dir in IGNORED_DIRS {
        let pattern = format!("!**/{dir}/");
        if let Err(e) = builder.add(&pattern) {
            tracing::warn!("failed to add ignore pattern {pattern}: {e}");
        }
    }

    builder.build().ok()
}

pub(crate) fn is_non_code_directory(path: &Path) -> bool {
    let path_str = path.as_os_str().to_str().unwrap_or("");
    IGNORED_DIRS.iter().any(|&dir| {
        #[cfg(target_os = "windows")]
        let dir = dir.replace('/', std::path::MAIN_SEPARATOR_STR);
        #[cfg(target_os = "windows")]
        return path_str.contains(dir.as_str());

        #[cfg(not(target_os = "windows"))]
        path_str.contains(dir)
    })
}

/// Caller-supplied ignore OVERLAY, layered on top of the ignore-file policy.
///
/// Lets an embedder express the two things ripgrep's `ignore` crate cannot:
/// an extra ignore-file NAME (`.rgignore`) and a per-project re-include of
/// paths a `.gitignore` dropped. The resulting decision is
///
/// ```text
/// excluded(f) = exclude_globs(f) OR (ignored(f) AND NOT unignore_globs(f))
/// ```
///
/// Both glob lists speak `.gitignore` pattern syntax (NOT ripgrep `-g`
/// override syntax, whose whitelist semantics are inverted and would drop
/// every non-matching file). Empty lists = no overlay, zero cost.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IgnoreOverlay {
    /// Extra ignore-file names honored in every directory, e.g. `.rgignore`.
    /// Applied only when ignore files are respected at all.
    pub custom_ignore_filenames: Vec<String>,
    /// Always dropped, whatever the ignore files say.
    pub exclude_globs: Vec<String>,
    /// Re-included even when an ignore file excludes them. A gitignored
    /// DIRECTORY is never descended by the walker, so these subtrees are
    /// collected by a second, ignore-free pass over each pattern's static
    /// prefix.
    pub unignore_globs: Vec<String>,
}

impl IgnoreOverlay {
    /// True when the overlay adds nothing (the common case).
    pub fn is_empty(&self) -> bool {
        self.custom_ignore_filenames.is_empty()
            && self.exclude_globs.is_empty()
            && self.unignore_globs.is_empty()
    }

    /// Compile the glob lists once, against `base_path`.
    pub fn compile(&self, base_path: &Path) -> CompiledIgnoreOverlay {
        CompiledIgnoreOverlay::new(self.clone(), base_path)
    }
}

/// The literal directory prefix of a `.gitignore`-style pattern: every leading
/// component up to the first one carrying a glob meta character. `**/vendored`
/// yields `""` — "any directory might match", so the whole tree is reopened.
#[cfg(any(feature = "ripgrep", test))]
fn glob_static_prefix(pattern: &str) -> String {
    let pattern = pattern.trim_start_matches('/');
    let mut out: Vec<&str> = Vec::new();
    for component in pattern.split('/') {
        if component.is_empty() {
            continue;
        }
        if component.contains(['*', '?', '[', ']', '{', '}', '!']) {
            break;
        }
        out.push(component);
    }
    out.join("/")
}

#[cfg(feature = "ripgrep")]
fn unignore_root(base_path: &Path, pattern: &str) -> Option<std::path::PathBuf> {
    let prefix = glob_static_prefix(pattern);
    let relative = Path::new(&prefix);
    relative
        .components()
        .all(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::Normal(_)
            )
        })
        .then(|| base_path.join(relative))
}
#[cfg(feature = "ripgrep")]
#[derive(Debug)]
pub struct CompiledIgnoreOverlay {
    overlay: IgnoreOverlay,
    exclude: Option<::ignore::gitignore::Gitignore>,
    unignore: Option<::ignore::gitignore::Gitignore>,
    unignore_roots: Vec<std::path::PathBuf>,
}

#[cfg(feature = "ripgrep")]
impl Default for CompiledIgnoreOverlay {
    fn default() -> Self {
        IgnoreOverlay::default().compile(Path::new(""))
    }
}

#[cfg(feature = "ripgrep")]
impl CompiledIgnoreOverlay {
    fn new(overlay: IgnoreOverlay, base_path: &Path) -> Self {
        let build = |patterns: &[String]| -> Option<::ignore::gitignore::Gitignore> {
            if patterns.is_empty() {
                return None;
            }
            let mut builder = ::ignore::gitignore::GitignoreBuilder::new(base_path);
            for pattern in patterns {
                if let Err(e) = builder.add_line(None, pattern) {
                    tracing::warn!("ignoring malformed overlay pattern {pattern}: {e}");
                }
            }
            builder.build().ok()
        };

        let unignore_roots = overlay
            .unignore_globs
            .iter()
            .filter_map(|pattern| unignore_root(base_path, pattern))
            .collect();

        Self {
            exclude: build(&overlay.exclude_globs),
            unignore: build(&overlay.unignore_globs),
            unignore_roots,
            overlay,
        }
    }

    /// The uncompiled overlay this was built from, for rebuilding against a
    /// new base path (e.g. on directory change).
    pub fn source(&self) -> &IgnoreOverlay {
        &self.overlay
    }

    pub fn is_empty(&self) -> bool {
        self.overlay.is_empty()
    }

    pub(crate) fn custom_ignore_filenames(&self) -> &[String] {
        &self.overlay.custom_ignore_filenames
    }

    /// Roots (absolute, deduped by the caller) that a second ignore-free pass
    /// must walk to surface the re-included files.
    pub(crate) fn unignore_roots(&self) -> &[std::path::PathBuf] {
        &self.unignore_roots
    }

    /// `rel` is base-relative and `/`-separated.
    pub(crate) fn is_excluded(&self, rel: &Path, is_dir: bool) -> bool {
        self.exclude
            .as_ref()
            .is_some_and(|g| g.matched_path_or_any_parents(rel, is_dir).is_ignore())
    }

    /// `rel` is base-relative and `/`-separated.
    pub(crate) fn is_unignored(&self, rel: &Path, is_dir: bool) -> bool {
        self.unignore
            .as_ref()
            .is_some_and(|g| g.matched_path_or_any_parents(rel, is_dir).is_ignore())
    }
}

// Without the `ripgrep` feature there is no gitignore matcher to compile
// against (the zlob backend brings its own globber), so the overlay degrades
// to "no overlay" instead of failing the build.
#[cfg(not(feature = "ripgrep"))]
#[derive(Debug)]
pub struct CompiledIgnoreOverlay {
    overlay: IgnoreOverlay,
}

#[cfg(not(feature = "ripgrep"))]
impl Default for CompiledIgnoreOverlay {
    fn default() -> Self {
        IgnoreOverlay::default().compile(Path::new(""))
    }
}

#[cfg(not(feature = "ripgrep"))]
impl CompiledIgnoreOverlay {
    fn new(overlay: IgnoreOverlay, _base_path: &Path) -> Self {
        if !overlay.is_empty() {
            tracing::warn!("ignore overlay requires the `ripgrep` feature; ignoring it");
        }
        Self { overlay }
    }

    /// The uncompiled overlay this was built from, for rebuilding against a
    /// new base path (e.g. on directory change).
    pub fn source(&self) -> &IgnoreOverlay {
        &self.overlay
    }

    pub fn is_empty(&self) -> bool {
        self.overlay.is_empty()
    }

    pub(crate) fn is_excluded(&self, _rel: &Path, _is_dir: bool) -> bool {
        false
    }

    pub(crate) fn is_unignored(&self, _rel: &Path, _is_dir: bool) -> bool {
        false
    }
}

#[cfg(test)]
mod overlay_tests {
    use super::*;

    #[test]
    fn static_prefixes() {
        assert_eq!(glob_static_prefix("repositories/**"), "repositories");
        assert_eq!(glob_static_prefix("/build/out/*.js"), "build/out");
        assert_eq!(glob_static_prefix("**/vendored/"), "");
        assert_eq!(glob_static_prefix("dist"), "dist");
    }

    #[cfg(feature = "ripgrep")]
    #[test]
    fn matches_directories_and_children() {
        let overlay = IgnoreOverlay {
            custom_ignore_filenames: vec![".rgignore".into()],
            exclude_globs: vec!["secrets/**".into()],
            unignore_globs: vec!["target/keep/**".into()],
        };
        let compiled = overlay.compile(Path::new("/repo"));
        assert!(!compiled.is_empty());
        assert!(compiled.is_excluded(Path::new("secrets/key.pem"), false));
        assert!(!compiled.is_excluded(Path::new("src/main.rs"), false));
        assert!(compiled.is_unignored(Path::new("target/keep/a.txt"), false));
        assert!(!compiled.is_unignored(Path::new("target/other/a.txt"), false));
        assert_eq!(compiled.unignore_roots(), [Path::new("/repo/target/keep")]);
        assert_eq!(
            compiled.custom_ignore_filenames(),
            [".rgignore".to_string()]
        );
    }
}
