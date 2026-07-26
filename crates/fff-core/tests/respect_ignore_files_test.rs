use std::fs;
use std::path::Path;
use std::process::Command;

use fff_search::FilePickerOptions;
use fff_search::file_picker::FilePicker;
use tempfile::TempDir;

fn init_repo(base: &Path) {
    let ok = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(base)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "git init failed");
}

fn write(base: &Path, rel: &str, contents: &str) {
    let full = base.join(rel);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(full, contents).unwrap();
}

fn indexed_paths(base: &Path, respect_ignore_files: bool) -> Vec<String> {
    let mut picker = FilePicker::new(FilePickerOptions {
        base_path: base.to_string_lossy().to_string(),
        enable_mmap_cache: false,
        enable_content_indexing: false,
        watch: false,
        respect_ignore_files,
        ..Default::default()
    })
    .expect("FilePicker::new");
    picker.collect_files().expect("collect_files");
    let mut paths: Vec<String> = picker
        .get_files()
        .iter()
        .map(|f| f.relative_path(&picker).replace('\\', "/"))
        .collect();
    paths.sort();
    paths
}

#[test]
fn respecting_ignore_files_drops_ignored_paths() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path();
    init_repo(base);
    write(base, ".gitignore", "target/\n*.log\n");
    write(base, "src/main.rs", "fn main() {}\n");
    write(base, "target/build.rs", "// generated\n");
    write(base, "noisy.log", "noise\n");

    let paths = indexed_paths(base, true);

    assert!(paths.iter().any(|p| p == "src/main.rs"), "{paths:?}");
    assert!(!paths.iter().any(|p| p == "target/build.rs"), "{paths:?}");
    assert!(!paths.iter().any(|p| p == "noisy.log"), "{paths:?}");
}

#[test]
fn ignoring_ignore_files_indexes_ignored_paths() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path();
    init_repo(base);
    write(base, ".gitignore", "target/\n*.log\n");
    write(base, "src/main.rs", "fn main() {}\n");
    write(base, "target/build.rs", "// generated\n");
    write(base, "noisy.log", "noise\n");

    let paths = indexed_paths(base, false);

    assert!(paths.iter().any(|p| p == "src/main.rs"), "{paths:?}");
    assert!(paths.iter().any(|p| p == "target/build.rs"), "{paths:?}");
    assert!(paths.iter().any(|p| p == "noisy.log"), "{paths:?}");
    // `.git` internals stay out even with every ignore file disabled.
    assert!(!paths.iter().any(|p| p.starts_with(".git/")), "{paths:?}");
}

#[test]
fn cache_dirs_stay_pruned_even_without_ignore_files() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path();
    init_repo(base);
    write(base, ".gitignore", "target/\n");
    write(base, "src/main.rs", "fn main() {}\n");
    write(base, "target/build.rs", "// generated\n");
    write(base, ".cpcache/main.clj.cache", "blob\n");
    write(base, ".clj-kondo/.cache/2025/lib.transit.json", "blob\n");
    write(base, ".clj-kondo/config.edn", "{}\n");
    write(base, "py/__pycache__/mod.pyc", "blob\n");

    let paths = indexed_paths(base, false);

    // Ignored SOURCE comes back …
    assert!(paths.iter().any(|p| p == "target/build.rs"), "{paths:?}");
    assert!(
        paths.iter().any(|p| p == ".clj-kondo/config.edn"),
        "{paths:?}"
    );
    // … machine-generated cache trees never do.
    assert!(!paths.iter().any(|p| p.contains(".cpcache/")), "{paths:?}");
    assert!(!paths.iter().any(|p| p.contains("/.cache/")), "{paths:?}");
    assert!(
        !paths.iter().any(|p| p.contains("__pycache__/")),
        "{paths:?}"
    );
}
