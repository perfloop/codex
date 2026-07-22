#![allow(clippy::expect_used)]

use codex_file_search::FileSearchOptions;
use codex_file_search::MatchType;
use codex_file_search::run;
use divan::Bencher;
use std::env;
use std::fs;
use std::hint::black_box;
use std::num::NonZero;
use std::path::Path;
use std::path::PathBuf;
use tempfile::TempDir;

const MATCH_LIMIT: usize = 20;
const WALK_THREADS: usize = 2;
const ENTRIES_PER_TYPE: usize = 1_024;

fn main() {
    if env::args().any(|arg| arg == "--perfloop-walk-shape-json") {
        emit_walk_shape();
    } else {
        divan::main();
    }
}

#[divan::bench(sample_count = 20, sample_size = 1)]
fn initial_walk_completion_pair(bencher: Bencher) {
    bencher
        .with_inputs(WalkFixture::new)
        .bench_local_values(|fixture| black_box(run_query_pair(&fixture)));
}

fn emit_walk_shape() {
    let shape = run_query_pair(&WalkFixture::new());
    println!(
        "{{\"metric\":\"initial_walk_dense_match_count\",\"value\":{}}}",
        shape.dense_match_count
    );
    println!(
        "{{\"metric\":\"initial_walk_no_match_count\",\"value\":{}}}",
        shape.no_match_count
    );
}

struct WalkFixture {
    _tree: TempDir,
    root: PathBuf,
}

impl WalkFixture {
    fn new() -> Self {
        let fixture_parent = Path::new("target/perfloop");
        fs::create_dir_all(fixture_parent).expect("benchmark fixture parent");
        let tree = tempfile::Builder::new()
            .prefix("file-search-walk-")
            .tempdir_in(fixture_parent)
            .expect("benchmark tree");
        create_dense_tree(tree.path());
        Self {
            root: tree.path().to_path_buf(),
            _tree: tree,
        }
    }
}

struct WalkShape {
    dense_match_count: usize,
    no_match_count: usize,
}

fn run_query_pair(fixture: &WalkFixture) -> WalkShape {
    let dense = run_query(&fixture.root, "dense");
    assert_eq!(dense.total_match_count, ENTRIES_PER_TYPE * 2);
    assert_eq!(dense.matches.len(), MATCH_LIMIT);
    assert!(dense.matches.iter().all(|file_match| {
        let name = file_match
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("ASCII fixture name");
        (name.starts_with("dense-file-") && file_match.match_type == MatchType::File)
            || (name.starts_with("dense-directory-")
                && file_match.match_type == MatchType::Directory)
    }));

    let no_match = run_query(&fixture.root, "not-a-fixture-entry");
    assert!(no_match.matches.is_empty());
    assert_eq!(no_match.total_match_count, 0);

    WalkShape {
        dense_match_count: dense.total_match_count,
        no_match_count: no_match.total_match_count,
    }
}

fn run_query(root: &Path, query: &str) -> codex_file_search::FileSearchResults {
    run(
        query,
        vec![root.to_path_buf()],
        FileSearchOptions {
            limit: NonZero::new(MATCH_LIMIT).expect("positive match limit"),
            exclude: Vec::new(),
            threads: NonZero::new(WALK_THREADS).expect("positive walker thread count"),
            compute_indices: false,
            respect_gitignore: true,
        },
        /*cancel_flag*/ None,
    )
    .expect("file search run")
}

fn create_dense_tree(root: &Path) {
    for index in 0..ENTRIES_PER_TYPE {
        fs::write(
            root.join(format!("dense-file-{index:04}.txt")),
            "benchmark fixture",
        )
        .expect("write fixture file");
        fs::create_dir(root.join(format!("dense-directory-{index:04}")))
            .expect("create fixture directory");
    }
}
