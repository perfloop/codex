use codex_file_search::MatchType;
use codex_tui::AppEvent;
use codex_tui::AppEventSender;
use codex_tui::FileSearchManager;
use divan::Bencher;
use std::fs;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedReceiver;

const DENSE_MATCH_COUNT: usize = 64;
const MATCH_LIMIT: usize = 20;
const QUERY_MARKER_LENGTH: usize = 200;

fn main() {
    divan::main();
}

#[divan::bench]
fn on_user_query_dense_result_delivery(bencher: Bencher) {
    let mut state = TuiQueryBench::new();
    bencher.bench_local(|| state.update_and_validate());
}

struct TuiQueryBench {
    manager: FileSearchManager,
    events: UnboundedReceiver<AppEvent>,
    _tree: tempfile::TempDir,
    query_generation: usize,
}

impl TuiQueryBench {
    fn new() -> Self {
        let tree =
            tempfile::tempdir().unwrap_or_else(|error| panic!("create fixture tree: {error}"));
        let marker = "a".repeat(QUERY_MARKER_LENGTH);
        for index in 0..DENSE_MATCH_COUNT {
            let path = tree
                .path()
                .join(format!("dense-{marker}-file-{index:03}.txt"));
            fs::write(path, format!("fixture {index}"))
                .unwrap_or_else(|error| panic!("write fixture: {error}"));
        }

        let (app_event_tx, events) = mpsc::unbounded_channel();
        let manager =
            FileSearchManager::new(tree.path().to_path_buf(), AppEventSender { app_event_tx });
        let mut state = Self {
            manager,
            events,
            _tree: tree,
            query_generation: 0,
        };
        divan::black_box(state.update_and_validate());
        state
    }

    fn update_and_validate(&mut self) -> usize {
        self.query_generation += 1;
        assert!(self.query_generation <= QUERY_MARKER_LENGTH);
        let query = format!("dense-{}", "a".repeat(self.query_generation));
        self.manager.on_user_query(query.clone());

        loop {
            match self.events.blocking_recv() {
                Some(AppEvent::FileSearchResult {
                    query: result_query,
                    matches,
                }) if result_query == query && matches.len() == MATCH_LIMIT => {
                    assert!(
                        matches
                            .iter()
                            .all(|file_match| file_match.match_type == MatchType::File)
                    );
                    return matches.len();
                }
                Some(_) => {}
                None => panic!("file-search event channel closed before delivering {query}"),
            }
        }
    }
}
