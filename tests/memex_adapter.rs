//! The memex backend against a fake `memex` script: argument building, output parsing,
//! over-fetching for `until`, and killing memex when a request is cancelled.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sessift::backend::memex::MemexBackend;
use sessift::model::{ReadRequest, SearchRequest, SessionsRequest};

const T0: i64 = 1_793_000_000_000;
const DAY: i64 = 86_400_000;

/// A `memex` stand-in. It logs its arguments, records its pid, and serves 50 sessions that are
/// one day apart, newest first, honouring `--limit`.
fn fake_memex(dir: &Path, search_sleep: u32) -> PathBuf {
    let path = dir.join("memex");
    let script = format!(
        r#"#!/bin/sh
echo $$ > '{pid}'
echo "$*" >> '{log}'
for a in "$@"; do case "$a" in --version) echo "memex 9.9.9"; exit 0;; esac; done
cmd=""; limit=20; prev=""
for a in "$@"; do
  case "$a" in search|sessions|session) [ -z "$cmd" ] && cmd=$a;; esac
  [ "$prev" = "--limit" ] && limit=$a
  prev=$a
done
case "$cmd" in
  search)
    sleep {search_sleep}
    printf '%s\n' '{{"score":2.5,"ts":"2026-10-01T00:00:00Z","record_id":"r1","project":"p","role":"user","session_id":"s1","source":"claude","source_path":"/s1.jsonl","snippet":"hello\n\nworld"}}'
    ;;
  sessions)
    i=0
    while [ $i -lt $limit ] && [ $i -lt 50 ]; do
      echo "{{\"source\":\"codex\",\"session_id\":\"s$i\",\"last_at\":$(( {t0} - i * {day} )),\"message_count\":$i}}"
      i=$((i+1))
    done
    ;;
  *) echo "memex: unexpected command $cmd" >&2; exit 2;;
esac
"#,
        pid = dir.join("pid").display(),
        log = dir.join("calls.log").display(),
        t0 = T0,
        day = DAY,
    );
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn backend(dir: &Path, search_sleep: u32) -> MemexBackend {
    let fake = fake_memex(dir, search_sleep);
    MemexBackend::new(Some(fake.to_str().unwrap()), None)
}

#[tokio::test]
async fn search_asks_the_local_index_and_parses_hits() {
    let dir = tempfile::tempdir().unwrap();
    let memex = backend(dir.path(), 0);
    let mut req = SearchRequest::new("-v hello");
    req.filters.source = Some("claude".into());
    let hits = memex.search(&req).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].snippet, "hello world");
    assert_eq!(hits[0].ts_ms, 1_790_812_800_000);
    let call = &calls(dir.path())[0];
    assert!(
        call.starts_with("--no-update-check --non-interactive search --machine local"),
        "{call}"
    );
    assert!(call.contains("--source claude"), "{call}");
    assert!(call.ends_with("-- -v hello"), "{call}");
    assert_eq!(memex.info().await.version, "9.9.9");
}

#[tokio::test]
async fn sessions_until_fetches_more_until_enough_remain() {
    let dir = tempfile::tempdir().unwrap();
    let memex = backend(dir.path(), 0);
    let mut req = SessionsRequest {
        limit: 5,
        ..SessionsRequest::default()
    };
    req.filters.until_ms = Some(T0 - 30 * DAY);
    let sessions = memex.sessions(&req).await.unwrap();
    let ids: Vec<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
    assert_eq!(ids, vec!["s30", "s31", "s32", "s33", "s34"]);
    let limits: Vec<String> = calls(dir.path())
        .iter()
        .map(|c| {
            c.split("--limit ")
                .nth(1)
                .unwrap()
                .split(' ')
                .next()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(limits, vec!["5", "20", "80"]);

    // Without `until` one call is enough, and offsets page the newest-first list.
    let dir = tempfile::tempdir().unwrap();
    let memex = backend(dir.path(), 0);
    let page = memex
        .sessions(&SessionsRequest {
            limit: 3,
            offset: 2,
            ..SessionsRequest::default()
        })
        .await
        .unwrap();
    let ids: Vec<&str> = page.iter().map(|s| s.session_id.as_str()).collect();
    assert_eq!(ids, vec!["s2", "s3", "s4"]);
    assert_eq!(calls(dir.path()).len(), 1);
}

#[tokio::test]
async fn cancelling_a_request_kills_memex() {
    let dir = tempfile::tempdir().unwrap();
    let memex = backend(dir.path(), 30);
    let request = SearchRequest::new("slow");
    let pid_file = dir.path().join("pid");
    // Drive the search until memex is running, then abandon it by dropping the future.
    let pid = {
        let search = memex.search(&request);
        tokio::pin!(search);
        let started = async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&pid_file)
                    && !pid.trim().is_empty()
                {
                    return pid.trim().to_string();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::select! {
            _ = &mut search => panic!("the slow search finished"),
            pid = started => pid,
        }
    };
    let pid = pid.as_str();
    // Give the kill a moment to land, then check the process is gone.
    let mut alive = true;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        alive = std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        if !alive {
            break;
        }
    }
    assert!(!alive, "memex {pid} is still running");
}

#[tokio::test]
async fn memex_errors_carry_its_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let memex = backend(dir.path(), 0);
    let fake = dir.path().join("memex");
    // Make `session` unknown to the fake so it fails like a broken memex would.
    let script = std::fs::read_to_string(&fake)
        .unwrap()
        .replace("search|sessions|session)", "search|sessions)");
    std::fs::write(&fake, script).unwrap();
    let err = memex.read(&ReadRequest::new("abc")).await.unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("memex exited with"), "{message}");
    assert!(message.contains("unexpected command"), "{message}");
}

#[tokio::test]
async fn a_missing_memex_is_reported_not_fatal() {
    let memex = MemexBackend::new(Some("/nonexistent/memex"), None);
    let err = memex.search(&SearchRequest::new("x")).await.unwrap_err();
    assert!(format!("{err:#}").contains("/nonexistent/memex"));
}
