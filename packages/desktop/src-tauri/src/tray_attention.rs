//! Linux tray attention is derived from the hub's saved unread/mute state.
//! Only the last known boolean per signed-in account is cached in memory; no
//! unread records are persisted. Popup preferences do not control this indicator.
use crate::notifications::{api, field, get, response, Frames};
use crate::{Active, Desktop};
use anyhow::{ensure, Context, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{atomic::Ordering, Arc};
use std::time::Duration;
use tauri::Manager;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const DISCOVER_EVERY: Duration = Duration::from_secs(5);
const REFRESH_EVERY: Duration = Duration::from_secs(30);
const RETRY_AFTER: Duration = Duration::from_secs(2);
const MAX_ACCOUNTS: usize = 64;
const MAX_PAGES: usize = 512;

#[derive(Default)]
struct Accounts {
    next_generation: u64,
    values: HashMap<String, (u64, bool)>,
}
impl Accounts {
    fn reconcile(&mut self, ids: &[String]) -> Vec<(String, u64)> {
        self.values.retain(|id, _| ids.contains(id));
        let mut added = Vec::new();
        for id in ids {
            if !self.values.contains_key(id) {
                self.next_generation += 1;
                self.values
                    .insert(id.clone(), (self.next_generation, false));
                added.push((id.clone(), self.next_generation));
            }
        }
        added
    }
    fn update(&mut self, id: &str, generation: u64, unread: bool) {
        if let Some((current, value)) = self.values.get_mut(id) {
            if *current == generation {
                *value = unread;
            }
        }
    }
    fn restart(&mut self, id: &str) -> Option<u64> {
        let value = self.values.get_mut(id)?;
        self.next_generation += 1;
        value.0 = self.next_generation;
        Some(self.next_generation)
    }
    fn unread(&self) -> bool {
        self.values.values().any(|(_, unread)| *unread)
    }
}

enum Update {
    Accounts(Arc<Active>, Vec<String>),
    Unread(Arc<Active>, String, u64, bool),
}

fn same_session(left: Option<&Arc<Active>>, right: Option<&Arc<Active>>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        (None, None) => true,
        _ => false,
    }
}

fn apply(app: &tauri::AppHandle, expected: Option<Arc<Active>>, unread: bool) -> Result<bool> {
    let tray = app
        .tray_by_id(crate::TRAY_ID)
        .context("Missing native tray")?;
    let app = app.clone();
    // Recheck on the UI thread: results queued by a previous hub must not set
    // the newly selected hub's status. A busy settings lock is retried later.
    tray.with_inner_tray_icon(move |tray| -> std::result::Result<bool, tray_icon::Error> {
        let desktop = app.state::<Desktop>();
        let Ok(inner) = desktop.inner.try_lock() else {
            return Ok(false);
        };
        if !same_session(inner.active.as_ref(), expected.as_ref()) {
            return Ok(false);
        }
        tray.set_status(if unread {
            tray_icon::TrayIconStatus::NeedsAttention
        } else {
            tray_icon::TrayIconStatus::Active
        })?;
        Ok(true)
    })?
    .map_err(Into::into)
}

pub(super) fn start(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(coordinate(app));
}

async fn coordinate(app: tauri::AppHandle) {
    let (sender, mut receiver) = mpsc::channel(128);
    let mut current: Option<Arc<Active>> = None;
    let mut discovery: Option<JoinHandle<()>> = None;
    let mut workers: HashMap<String, JoinHandle<()>> = HashMap::new();
    let mut accounts = Accounts::default();
    let mut rendered = None;
    loop {
        let desktop = app.state::<Desktop>();
        if desktop.quit.load(Ordering::Acquire) != 0 {
            break;
        }
        let active = desktop.inner.lock().await.active.clone();
        if !same_session(current.as_ref(), active.as_ref()) {
            if let Some(task) = discovery.take() {
                task.abort();
            }
            for (_, task) in workers.drain() {
                task.abort();
            }
            accounts = Accounts::default();
            rendered = None;
            current = active;
            if let Some(active) = current.clone() {
                discovery = Some(tokio::spawn(discover(active, sender.clone())));
            }
        }
        let unread = accounts.unread();
        if rendered != Some(unread) && matches!(apply(&app, current.clone(), unread), Ok(true)) {
            rendered = Some(unread);
        }
        let update = tokio::select! {
            update = receiver.recv() => update,
            _ = tokio::time::sleep(Duration::from_secs(1)) => None,
        };
        match update {
            Some(Update::Accounts(active, ids))
                if same_session(current.as_ref(), Some(&active)) =>
            {
                let mut added = accounts.reconcile(&ids);
                workers.retain(|id, task| {
                    if !accounts.values.contains_key(id) {
                        task.abort();
                        false
                    } else if task.is_finished() {
                        // Restarting a listener does not prove its unread chat
                        // was read; retain the value but fence queued updates.
                        if let Some(generation) = accounts.restart(id) {
                            added.push((id.clone(), generation));
                        }
                        false
                    } else {
                        true
                    }
                });
                for (id, generation) in added {
                    workers.insert(
                        id.clone(),
                        tokio::spawn(watch(active.clone(), id, generation, sender.clone())),
                    );
                }
            }
            Some(Update::Unread(active, id, generation, unread))
                if same_session(current.as_ref(), Some(&active)) =>
            {
                accounts.update(&id, generation, unread);
            }
            _ => {}
        }
    }
    if let Some(task) = discovery {
        task.abort();
    }
    for (_, task) in workers {
        task.abort();
    }
}

fn signed_in(page: &Value) -> Result<Vec<String>> {
    let items = page["items"].as_array().context("Missing accounts")?;
    let mut ids = Vec::new();
    for account in items {
        match account["authenticated"].as_bool() {
            Some(true) => ids.push(field(account, "id")?.to_owned()),
            Some(false) => {}
            None => anyhow::bail!("Unknown account authentication state"),
        }
    }
    ids.sort();
    ids.dedup();
    ensure!(
        ids.len() <= MAX_ACCOUNTS,
        "Too many signed-in tray accounts"
    );
    Ok(ids)
}

async fn discover(active: Arc<Active>, sender: mpsc::Sender<Update>) {
    loop {
        if let Ok(url) = api(&active, &["v1", "accounts"]) {
            if let Ok(page) = get(&active, url).await {
                if let Ok(ids) = signed_in(&page) {
                    if sender
                        .send(Update::Accounts(active.clone(), ids))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
        tokio::time::sleep(DISCOVER_EVERY).await;
    }
}

fn thread_unread(thread: &Value) -> Option<bool> {
    if thread["deleted"] == true || thread["muted"] == true {
        return Some(false);
    }
    match thread["unreadCount"].as_i64() {
        Some(0) => Some(false),
        Some(count) if count > 0 && thread["muted"] == false => Some(true),
        _ => None,
    }
}

struct Snapshot {
    unread: Option<bool>,
    cursor: u64,
}

async fn snapshot(active: &Active, account: &str) -> Result<Snapshot> {
    let mut generation = None;
    let mut boundary = None;
    let mut next: Option<String> = None;
    let mut seen = HashSet::new();
    let mut complete = true;
    for _ in 0..MAX_PAGES {
        let mut url = api(active, &["v1", "accounts", account, "threads"])?;
        url.query_pairs_mut().append_pair("limit", "200");
        if let Some(cursor) = &next {
            url.query_pairs_mut().append_pair("cursor", cursor);
        }
        let page = get(active, url).await?;
        let source = field(&page, "sourceGeneration")?;
        if let Some(generation) = &generation {
            ensure!(source == generation, "Thread snapshot changed while paging");
        } else {
            generation = Some(source.to_owned());
            boundary = Some(field(&page, "asOfEventCursor")?.parse::<u64>()?);
        }
        complete &= page["coverage"]["partial"].as_bool() == Some(false);
        for thread in page["items"].as_array().context("Missing threads")? {
            match thread_unread(thread) {
                Some(true) => {
                    return Ok(Snapshot {
                        unread: Some(true),
                        cursor: boundary.context("Missing snapshot boundary")?,
                    });
                }
                Some(false) => {}
                None => complete = false,
            }
        }
        let cursor = match page.get("nextCursor") {
            Some(Value::Null) => {
                return Ok(Snapshot {
                    unread: complete.then_some(false),
                    cursor: boundary.context("Missing snapshot boundary")?,
                });
            }
            Some(Value::String(cursor)) => cursor.clone(),
            _ => anyhow::bail!("Missing thread page cursor"),
        };
        ensure!(
            !cursor.is_empty() && cursor.len() <= 8192 && seen.insert(cursor.clone()),
            "Invalid thread page cursor"
        );
        next = Some(cursor);
    }
    anyhow::bail!("Tray snapshot exceeds page bound")
}

fn affects_unread(event: &Value) -> bool {
    event["kind"].as_str().is_some_and(|kind| {
        [
            "message.", "thread.", "contact.", "group.", "account.", "sync.",
        ]
        .iter()
        .any(|prefix| kind.starts_with(prefix))
    })
}

async fn changed(active: &Active, account: &str, after: u64) -> Result<()> {
    let mut url = api(active, &["v1", "accounts", account, "events", "stream"])?;
    url.query_pairs_mut()
        .append_pair("after", &after.to_string());
    let mut response = response(active, url).await?.error_for_status()?;
    let deadline = tokio::time::Instant::now() + REFRESH_EVERY;
    let mut parser = Frames::default();
    loop {
        let chunk = tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return Ok(()),
            chunk = response.chunk() => chunk?,
        };
        let Some(chunk) = chunk else {
            anyhow::bail!("Tray event stream closed");
        };
        for frame in parser.push(&chunk)? {
            if frame.get("error").is_some() {
                anyhow::bail!("Tray event stream reset");
            }
            if field(&frame, "cursor")?.parse::<u64>()? > after && affects_unread(&frame) {
                return Ok(());
            }
        }
    }
}

async fn watch(
    active: Arc<Active>,
    account: String,
    generation: u64,
    sender: mpsc::Sender<Update>,
) {
    let mut published = None;
    loop {
        let result = tokio::time::timeout(REFRESH_EVERY, snapshot(&active, &account)).await;
        let Ok(Ok(snapshot)) = result else {
            // Missing/partial profile data and transport failures are not proof
            // that a previously unread chat was read. Keep the known value.
            tokio::time::sleep(RETRY_AFTER).await;
            continue;
        };
        if let Some(unread) = snapshot.unread.filter(|unread| published != Some(*unread)) {
            if sender
                .send(Update::Unread(
                    active.clone(),
                    account.clone(),
                    generation,
                    unread,
                ))
                .await
                .is_err()
            {
                return;
            }
            published = Some(unread);
        }
        // Coalesce event bursts without delaying the coordinator or other accounts.
        let delay = if changed(&active, &account, snapshot.cursor).await.is_ok() {
            Duration::from_millis(150)
        } else {
            RETRY_AFTER
        };
        tokio::time::sleep(delay).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Connection;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Instant;
    use zeroize::Zeroizing;

    fn active(endpoint: String) -> Active {
        Active {
            connection: Connection {
                id: "fixture".into(),
                name: "fixture".into(),
                endpoint,
                local: false,
                server_id: Some("fixture-server".into()),
                certificate: None,
            },
            key: Zeroizing::new("synthetic-key".into()),
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
        }
    }

    // Synthetic HTTP/SSE only: never contacts WeChat or reads a profile.
    fn hub(replies: Vec<(&str, String)>) -> (Active, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let replies: Vec<_> = replies
            .into_iter()
            .map(|(path, body)| (path.to_owned(), body))
            .collect();
        let task = thread::spawn(move || {
            for (path, body) in replies {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "missing fixture request: {path}");
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line.trim_end(), format!("GET {path} HTTP/1.1"));
                loop {
                    line.clear();
                    assert!(reader.read_line(&mut line).unwrap() != 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                drop(reader);
                let kind = if body.starts_with("data:") {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        (active(format!("http://{address}")), task)
    }

    fn page(items: Value, next: Value, partial: bool) -> Value {
        json!({"items":items,"nextCursor":next,"sourceGeneration":"fixture-generation",
            "asOfEventCursor":"10","coverage":{"partial":partial,"databases":[]}})
    }

    #[test]
    fn only_known_unmuted_unread_threads_request_attention() {
        for (thread, expected) in [
            (json!({"unreadCount":2,"muted":false}), Some(true)),
            (json!({"unreadCount":2,"muted":true}), Some(false)),
            (json!({"unreadCount":0}), Some(false)),
            (json!({"unreadCount":null,"muted":true}), Some(false)),
            (json!({"deleted":true}), Some(false)),
            (json!({"unreadCount":2,"muted":null}), None),
            (json!({"unreadCount":null,"muted":false}), None),
            (json!({"unreadCount":-1,"muted":false}), None),
        ] {
            assert_eq!(thread_unread(&thread), expected);
        }
    }

    #[test]
    fn aggregates_accounts_and_rejects_results_from_a_previous_login() {
        let mut state = Accounts::default();
        let added = state.reconcile(&["one".into(), "two".into()]);
        state.update("one", added[0].1, true);
        state.update("two", added[1].1, true);
        state.update("one", added[0].1, false);
        assert!(state.unread(), "reading one account must not clear another");
        state.reconcile(&["one".into()]);
        assert!(
            !state.unread(),
            "logging out removes that account's attention"
        );
        let replacement = state.reconcile(&["one".into(), "two".into()]);
        state.update("two", added[1].1, true);
        assert!(
            !state.unread(),
            "a queued result from the old worker is stale"
        );
        state.update("two", replacement[0].1, true);
        assert!(state.unread());
    }

    #[test]
    fn restarting_a_listener_preserves_unread_but_fences_its_old_results() {
        let mut state = Accounts::default();
        let added = state.reconcile(&["one".into()]);
        let original = added[0].1;
        state.update("one", original, true);
        let replacement = state.restart("one").unwrap();
        state.update("one", original, false);
        assert!(state.unread());
        state.update("one", replacement, false);
        assert!(!state.unread());
    }

    #[test]
    fn session_identity_uses_the_connection_generation() {
        let first = Arc::new(active("http://127.0.0.1".into()));
        let replacement = Arc::new(active("http://127.0.0.1".into()));
        assert!(same_session(Some(&first), Some(&first.clone())));
        assert!(!same_session(Some(&first), Some(&replacement)));
        assert!(!same_session(Some(&first), None));
        assert!(same_session(None, None));
    }

    #[test]
    fn discovery_excludes_signed_out_accounts_without_guessing_unknown_state() {
        assert_eq!(
            signed_in(&json!({"items":[
                {"id":"one","authenticated":false},{"id":"two","authenticated":true}
            ]}))
            .unwrap(),
            vec!["two"]
        );
        assert!(signed_in(&json!({"items":[{"id":"one"}]})).is_err());
        assert!(signed_in(&json!({"items":[{"id":"","authenticated":true}]})).is_err());
    }

    #[tokio::test]
    async fn paginates_past_read_threads_and_keeps_the_first_event_boundary() {
        let first = page(json!([{"unreadCount":0}]), json!("next"), false);
        let mut second = page(json!([{"unreadCount":2,"muted":false}]), Value::Null, false);
        second["asOfEventCursor"] = json!("99");
        let (active, server) = hub(vec![
            ("/v1/accounts/one/threads?limit=200", first.to_string()),
            (
                "/v1/accounts/one/threads?limit=200&cursor=next",
                second.to_string(),
            ),
        ]);
        let result = snapshot(&active, "one").await.unwrap();
        server.join().unwrap();
        assert_eq!(result.unread, Some(true));
        assert_eq!(result.cursor, 10);
    }

    #[tokio::test]
    async fn partial_or_unknown_data_cannot_establish_that_everything_is_read() {
        for value in [
            page(json!([]), Value::Null, true),
            page(json!([{"unreadCount":3,"muted":null}]), Value::Null, false),
        ] {
            let (active, server) = hub(vec![(
                "/v1/accounts/one/threads?limit=200",
                value.to_string(),
            )]);
            assert_eq!(snapshot(&active, "one").await.unwrap().unread, None);
            server.join().unwrap();
        }
        let value = page(json!([{"unreadCount":1,"muted":false}]), Value::Null, true);
        let (active, server) = hub(vec![(
            "/v1/accounts/one/threads?limit=200",
            value.to_string(),
        )]);
        assert_eq!(snapshot(&active, "one").await.unwrap().unread, Some(true));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn a_changed_profile_generation_or_repeated_cursor_invalidates_the_scan() {
        for change_generation in [true, false] {
            let first = page(json!([]), json!("next"), false);
            let mut second = page(json!([]), json!("next"), false);
            if change_generation {
                second["sourceGeneration"] = json!("different");
            }
            let (active, server) = hub(vec![
                ("/v1/accounts/one/threads?limit=200", first.to_string()),
                (
                    "/v1/accounts/one/threads?limit=200&cursor=next",
                    second.to_string(),
                ),
            ]);
            assert!(snapshot(&active, "one").await.is_err());
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn read_events_refresh_unknown_snapshots_without_clearing_early() {
        let unread = page(json!([{"unreadCount":2,"muted":false}]), Value::Null, false);
        let mut unknown = page(json!([]), Value::Null, true);
        unknown["asOfEventCursor"] = json!("11");
        let mut read = page(json!([{"unreadCount":0}]), Value::Null, false);
        read["asOfEventCursor"] = json!("12");
        let event = |cursor: &str| {
            format!(
                "data: {}\n\n",
                json!({"kind":"thread.read-state","cursor":cursor})
            )
        };
        let (active, server) = hub(vec![
            ("/v1/accounts/one/threads?limit=200", unread.to_string()),
            ("/v1/accounts/one/events/stream?after=10", event("11")),
            ("/v1/accounts/one/threads?limit=200", unknown.to_string()),
            ("/v1/accounts/one/events/stream?after=11", event("12")),
            ("/v1/accounts/one/threads?limit=200", read.to_string()),
        ]);
        let active = Arc::new(active);
        let (sender, mut receiver) = mpsc::channel(8);
        let worker = tokio::spawn(watch(active.clone(), "one".into(), 7, sender));
        for expected in [true, false] {
            let update = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            match update {
                Update::Unread(session, id, generation, unread) => {
                    assert!(Arc::ptr_eq(&session, &active));
                    assert_eq!(id, "one");
                    assert_eq!(generation, 7);
                    assert_eq!(unread, expected);
                }
                _ => panic!("unexpected update"),
            }
        }
        worker.abort();
        server.join().unwrap();
    }

    #[test]
    fn profile_metadata_changes_invalidate_unread_state() {
        for kind in [
            "thread.read-state",
            "thread.updated",
            "message.committed",
            "contact.updated",
            "group.updated",
        ] {
            assert!(affects_unread(&json!({"kind":kind})));
        }
        assert!(!affects_unread(&json!({"kind":"heartbeat"})));
    }
}
