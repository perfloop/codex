use super::*;
use crate::app_event::AppEvent;
use crate::bottom_pane::AppEventSender;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::unbounded_channel;

fn composer() -> (ChatComposer, UnboundedReceiver<AppEvent>) {
    let (tx, rx) = unbounded_channel();
    (
        ChatComposer::new(
            true,
            AppEventSender::new(tx),
            false,
            "Ask Codex to do anything".into(),
            false,
        ),
        rx,
    )
}

fn file_search_queries(rx: &mut UnboundedReceiver<AppEvent>) -> Vec<String> {
    let mut queries = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AppEvent::StartFileSearch(query) = event
            && !query.is_empty()
            && queries.last() != Some(&query)
        {
            queries.push(query);
        }
    }
    queries
}

#[test]
fn file_search_emission_uses_each_non_burst_prefix_and_one_burst_result() {
    let (mut typed, mut typed_events) = composer();
    for ch in ['@', 'z', 'z'] {
        let _ = typed.handle_key_event(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        std::thread::sleep(ChatComposer::recommended_paste_flush_delay());
        assert!(typed.flush_paste_burst_if_due());
    }
    for _ in 0..2 {
        let _ = typed.handle_key_event(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    }
    for ch in ['a', 'b', 'c'] {
        let _ = typed.handle_key_event(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        std::thread::sleep(ChatComposer::recommended_paste_flush_delay());
        assert!(typed.flush_paste_burst_if_due());
    }
    assert_eq!(
        file_search_queries(&mut typed_events),
        ["z", "zz", "azz", "abzz", "abczz"]
    );

    let (mut pasted, mut pasted_events) = composer();
    let now = Instant::now();
    for (index, ch) in ['@', 'z', 'a', 'b'].into_iter().enumerate() {
        let _ = pasted.handle_input_basic_with_time(
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            now + Duration::from_millis(index as u64),
        );
        pasted.sync_popups();
    }
    assert!(pasted.handle_paste_burst_flush(now + Duration::from_secs(1)));
    assert_eq!(file_search_queries(&mut pasted_events), ["zab"]);
}
