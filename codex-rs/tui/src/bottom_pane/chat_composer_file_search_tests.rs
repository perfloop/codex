use super::*;
use crate::app_event::AppEvent;
use crate::bottom_pane::AppEventSender;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::unbounded_channel;

fn new_composer() -> (ChatComposer, UnboundedReceiver<AppEvent>) {
    let (tx, rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);
    (
        ChatComposer::new(
            /*has_input_focus*/ true,
            sender,
            /*enhanced_keys_supported*/ false,
            "Ask Codex to do anything".to_string(),
            /*disable_paste_burst*/ false,
        ),
        rx,
    )
}

// Mirror FileSearchManager's changed, nonempty query filter.
fn file_search_manager_queries(receiver: &mut UnboundedReceiver<AppEvent>) -> Vec<String> {
    let mut queries = Vec::new();
    let mut last_query = None;
    loop {
        match receiver.try_recv() {
            Ok(AppEvent::StartFileSearch(query))
                if !query.is_empty() && last_query.as_ref() != Some(&query) =>
            {
                last_query = Some(query.clone());
                queries.push(query);
            }
            Ok(_) => {}
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return queries,
        }
    }
}

#[test]
fn file_search_emission_uses_each_non_burst_prefix_and_one_burst_result() {
    // Delay past PasteBurst's threshold so each changed token is forwarded.
    let (mut typed_composer, mut typed_events) = new_composer();
    for ch in ['@', 'z', 'z'] {
        let _ =
            typed_composer.handle_key_event(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        std::thread::sleep(ChatComposer::recommended_paste_flush_delay());
        assert!(
            typed_composer.flush_paste_burst_if_due(),
            "expected the non-burst character {ch:?} to flush"
        );
    }
    // Cursor-before-suffix edits are non-append matcher inputs.
    for _ in 0..2 {
        let _ = typed_composer.handle_key_event(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    }
    for ch in ['a', 'b', 'c'] {
        let _ =
            typed_composer.handle_key_event(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        std::thread::sleep(ChatComposer::recommended_paste_flush_delay());
        assert!(
            typed_composer.flush_paste_burst_if_due(),
            "expected the non-burst character {ch:?} to flush"
        );
    }
    assert_eq!(
        file_search_manager_queries(&mut typed_events),
        vec!["z", "zz", "azz", "abzz", "abczz"],
        "each changed non-burst @ token must reach the file-search event path"
    );

    // A 1-ms paste-like stream publishes only its final token.
    let (mut burst_composer, mut burst_events) = new_composer();
    let mut now = Instant::now();
    for ch in ['@', 'z', 'a', 'b'] {
        let _ = burst_composer.handle_input_basic_with_time(
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            now,
        );
        burst_composer.sync_popups();
        now += Duration::from_millis(1);
    }
    assert!(
        burst_composer.handle_paste_burst_flush(now + Duration::from_secs(1)),
        "expected the fast stream to flush as one paste"
    );
    assert_eq!(
        file_search_manager_queries(&mut burst_events),
        vec!["zab"],
        "a burst paste must publish only its final @ token"
    );
}
