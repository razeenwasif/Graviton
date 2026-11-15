//! The engine's event log: containers starting, stopping and failing, and
//! images pulled and built. Kept in memory (the last [`KEEP`]) and appended to
//! `events.jsonl`, so the Activity feed survives restarts. Listeners (push
//! alerts) hear each event as it happens.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use graviton_proto::{EngineEvent, EventKind, decode_line, encode_line};

const KEEP: usize = 500;

type Listener = Box<dyn Fn(&EngineEvent) + Send + Sync>;

pub struct EventLog {
    path: PathBuf,
    state: Mutex<(VecDeque<EngineEvent>, u64)>,
    listeners: Mutex<Vec<Listener>>,
}

impl EventLog {
    pub fn open(path: PathBuf) -> Self {
        let mut events: VecDeque<EngineEvent> = File::open(&path)
            .map(|f| BufReader::new(f).lines().map_while(Result::ok).filter_map(|l| decode_line(l.as_bytes()).ok()).collect())
            .unwrap_or_default();
        let total = events.len();
        while events.len() > KEEP {
            events.pop_front();
        }
        // Keep the file from growing for ever: rewrite it with what's kept.
        if total > KEEP * 4 {
            let mut out = Vec::new();
            for e in &events {
                out.extend(encode_line(e));
            }
            let tmp = path.with_extension("jsonl.tmp");
            if fs::write(&tmp, out).is_ok() {
                let _ = fs::rename(tmp, &path);
            }
        }
        let next = events.back().map_or(1, |e| e.seq + 1);
        Self { path, state: Mutex::new((events, next)), listeners: Mutex::new(Vec::new()) }
    }

    pub fn subscribe(&self, listener: impl Fn(&EngineEvent) + Send + Sync + 'static) {
        self.listeners.lock().unwrap().push(Box::new(listener));
    }

    pub fn record(&self, kind: EventKind, container: Option<(&str, &str)>, image: Option<&str>, message: String) -> EngineEvent {
        let event = {
            let mut state = self.state.lock().unwrap();
            let event = EngineEvent {
                seq: state.1,
                timestamp_ms: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64),
                kind,
                container_id: container.map(|(id, _)| id.to_string()),
                container: container.map(|(_, name)| name.to_string()),
                image: image.map(String::from),
                message,
            };
            state.1 += 1;
            state.0.push_back(event.clone());
            while state.0.len() > KEEP {
                state.0.pop_front();
            }
            event
        };
        if let Some(dir) = self.path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = f.write_all(&encode_line(&event));
        }
        for listener in self.listeners.lock().unwrap().iter() {
            listener(&event);
        }
        event
    }

    /// Events after `since`, oldest first, at most `limit` (the newest ones).
    pub fn since(&self, since: Option<u64>, limit: Option<usize>) -> Vec<EngineEvent> {
        let state = self.state.lock().unwrap();
        let mut list: Vec<EngineEvent> = state.0.iter().filter(|e| since.is_none_or(|s| e.seq > s)).cloned().collect();
        let limit = limit.unwrap_or(KEEP).min(KEEP);
        if list.len() > limit {
            list.drain(..list.len() - limit);
        }
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_reloads() {
        let dir = std::env::temp_dir().join(format!("graviton-events-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let log = EventLog::open(dir.join("events.jsonl"));
        let heard = std::sync::Arc::new(Mutex::new(Vec::new()));
        let h = std::sync::Arc::clone(&heard);
        log.subscribe(move |e| h.lock().unwrap().push(e.seq));
        log.record(EventKind::Started, Some(("abc", "web")), Some("nginx"), "web started".into());
        log.record(EventKind::Failed, Some(("abc", "web")), None, "web exited with code 1".into());
        log.record(EventKind::Pulled, None, Some("redis:7"), "pulled redis:7".into());
        assert_eq!(*heard.lock().unwrap(), [1, 2, 3]);
        assert_eq!(log.since(Some(1), None).iter().map(|e| e.seq).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(log.since(None, Some(1))[0].kind, EventKind::Pulled);

        let reopened = EventLog::open(dir.join("events.jsonl"));
        assert_eq!(reopened.since(None, None).len(), 3);
        assert_eq!(reopened.record(EventKind::Removed, None, None, "x".into()).seq, 4);
        let _ = fs::remove_dir_all(dir);
    }
}
