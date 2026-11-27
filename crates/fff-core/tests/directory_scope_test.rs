use fff_query_parser::{AiGrepConfig, FFFQuery, FileSearchConfig};
use fff_search::file_picker::FilePicker;
use fff_search::{FilePickerOptions, FuzzySearchOptions, GrepSearchOptions, PaginationArgs};
use std::fs;

#[test]
fn literal_directory_scope_filters_before_paging() {
    let tmp = tempfile::tempdir().unwrap();
    for path in [
        "src/a.txt",
        "src/b.txt",
        "src2/noise.txt",
        "other/src/noise.txt",
        "space [x]/hit.txt",
    ] {
        let file = tmp.path().join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, "/src/\n").unwrap();
    }
    let mut picker = FilePicker::new(FilePickerOptions {
        base_path: tmp.path().to_string_lossy().into_owned(),
        ..Default::default()
    })
    .unwrap();
    picker.collect_files().unwrap();

    let query = FFFQuery::parse("", FileSearchConfig);
    let page = |scope, offset| {
        let result = picker.fuzzy_search_in_directory(
            &query,
            None,
            FuzzySearchOptions {
                pagination: PaginationArgs { offset, limit: 1 },
                ..Default::default()
            },
            scope,
        );
        (
            result.total_matched,
            result
                .items
                .iter()
                .map(|file| file.relative_path(&picker))
                .collect::<Vec<_>>(),
        )
    };
    assert_eq!(page("src", 0), (2, vec!["src/a.txt".into()]));
    assert_eq!(page("src", 1), (2, vec!["src/b.txt".into()]));
    assert_eq!(page("src", 2), (2, vec![]));
    assert_eq!(page("space [x]", 0), (1, vec!["space [x]/hit.txt".into()]));

    let query = FFFQuery::parse("/src/", AiGrepConfig);
    let grep = |offset| {
        picker.grep_in_directory(
            &query,
            &GrepSearchOptions {
                page_limit: 1,
                file_offset: offset,
                ..Default::default()
            },
            "src",
        )
    };
    let first = grep(0);
    assert_eq!(first.files[0].relative_path(&picker), "src/a.txt");
    assert_eq!(first.next_file_offset, 1);
    let second = grep(first.next_file_offset);
    assert_eq!(second.files[0].relative_path(&picker), "src/b.txt");
    assert_eq!(second.next_file_offset, 0);
}
