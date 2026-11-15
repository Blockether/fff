use std::fs;
use tempfile::TempDir;

use fff_search::FilePickerOptions;
use fff_search::file_picker::FilePicker;
use fff_search::grep::{GrepSearchOptions, parse_grep_query};

// The index learns of a change after the change. A search in that window must
// read the file as it is now: a mapping of the old length raises SIGBUS past
// the new end and ends the whole process.
#[test]
fn grep_reads_a_file_that_shrank_after_the_last_search() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("notes.txt");
    // Longer than one memory page on every platform.
    let old = format!("needle before\n{}", "filler line\n".repeat(8 * 1024));
    fs::write(&path, old).unwrap();
    let mut picker = FilePicker::new(FilePickerOptions {
        base_path: tmp.path().to_string_lossy().into_owned(),
        watch: false,
        ..Default::default()
    })
    .unwrap();
    picker.collect_files().unwrap();
    let options = GrepSearchOptions::default();
    let query = parse_grep_query("needle");
    assert_eq!(picker.grep(&query, &options).matches.len(), 1);

    fs::write(&path, "needle after\n").unwrap();

    let result = picker.grep(&query, &options);
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].line_content, "needle after");
}
