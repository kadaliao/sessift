//! The native index end to end, over the synthetic transcripts in tests/fixtures/home.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sessift::index::IndexPaths;
use sessift::index::ingest::Options;
use sessift::index::query::NativeIndex;
use sessift::index::source::Roots;
use sessift::model::{ContextRequest, Filters, ReadRequest, SearchRequest, SessionsRequest};

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    index: NativeIndex,
}

/// A copy of the fixture home (so tests can change transcripts) and an empty index.
fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/home"),
        &home,
    );
    // OpenCode keeps its sessions in SQLite; build the fixture database from SQL.
    let opencode = home.join(".local/share/opencode");
    std::fs::create_dir_all(&opencode).unwrap();
    let conn = rusqlite::Connection::open(opencode.join("opencode.db")).unwrap();
    for file in ["schema.sql", "insert.sql"] {
        let sql = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/opencode")
                .join(file),
        )
        .unwrap();
        conn.execute_batch(&sql).unwrap();
    }
    drop(conn);
    let index = NativeIndex::new(
        IndexPaths::new(dir.path().join("index")),
        Roots::standard(&home),
        Some(Duration::ZERO),
    );
    Env {
        _dir: dir,
        home,
        index,
    }
}

fn pi_dir(home: &Path) -> PathBuf {
    home.join(".pi/agent/sessions/--Users-alice-work-blog--")
}

fn pi_file(home: &Path, prefix: &str) -> PathBuf {
    std::fs::read_dir(pi_dir(home))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with(prefix))
        .unwrap()
}

fn search(index: &NativeIndex, query: &str) -> Vec<sessift::model::SearchHit> {
    index.search(&SearchRequest::new(query)).unwrap()
}

#[test]
fn chinese_word_mid_sentence_is_found_and_shown_in_place() {
    let env = env();
    let hits = search(&env.index, "回滚");
    for source in ["claude", "codex", "pi", "opencode"] {
        let hit = hits
            .iter()
            .find(|h| h.source == source && h.snippet.contains("在那之前回滚还来得及"))
            .unwrap_or_else(|| panic!("no {source} hit for 回滚: {hits:#?}"));
        assert_eq!(hit.role, "assistant");

        let mut req = ContextRequest::new(&hit.record_id);
        req.before = 1;
        req.after = 1;
        let page = env.index.context(&req).unwrap();
        let anchor = page
            .records
            .iter()
            .position(|r| r.record_id == hit.record_id)
            .unwrap();
        assert_eq!(anchor, 1, "one record before the hit");
        assert!(page.records[anchor].text.contains("在那之前回滚还来得及"));
    }
    let pi = hits.iter().find(|h| h.source == "pi").unwrap();
    assert_eq!(pi.project, "blog");
    assert_eq!(pi.session_id, "0190f3a2-1c00-7000-8000-00000000a001");
}

/// Every session of a source, whatever its kind.
fn sessions_of(index: &NativeIndex, source: &str) -> Vec<sessift::model::SessionSummary> {
    index
        .sessions(&SessionsRequest {
            filters: Filters {
                source: Some(source.into()),
                kind: Some("all".into()),
                ..Filters::default()
            },
            ..SessionsRequest::default()
        })
        .unwrap()
}

#[test]
fn sessions_list_with_titles_kinds_and_resume_commands() {
    let env = env();
    let all = env.index.sessions(&SessionsRequest::default()).unwrap();
    assert!(all.windows(2).all(|w| w[0].last_ms >= w[1].last_ms));
    // By default only conversations: no subagents, nothing without messages.
    assert!(!all.is_empty());
    assert!(all.iter().all(|s| matches!(
        s.kind.as_deref(),
        None | Some("main" | "fork" | "background")
    ) && s.message_count.unwrap_or(0) > 0));

    let sessions = sessions_of(&env.index, "pi");
    assert_eq!(sessions.len(), 3, "{sessions:#?}");
    let main = sessions
        .iter()
        .find(|s| s.kind.as_deref() == Some("main"))
        .unwrap();
    assert_eq!(
        main.title.as_deref(),
        Some("/skill:blog-deploy 把发布脚本改成先备份再上传")
    );
    assert_eq!(main.cwd.as_deref(), Some("/Users/alice/work/blog"));
    let resume = main.resume_cmd.clone().unwrap();
    assert!(resume.starts_with("pi --session "), "{resume}");
    assert!(
        sessions
            .iter()
            .any(|s| s.kind.as_deref() == Some("subagent"))
    );

    let claude = sessions_of(&env.index, "claude");
    assert_eq!(claude.len(), 4, "{claude:#?}");
    let shop = claude
        .iter()
        .find(|s| s.session_id == "5f0c6a1e-8b2d-4e7f-a913-2c4d6e8f0a1b")
        .unwrap();
    assert_eq!(shop.project, "shop-api");
    assert_eq!(
        shop.resume_cmd.as_deref(),
        Some(
            "cd /Users/alice/work/shop-api && claude --resume 5f0c6a1e-8b2d-4e7f-a913-2c4d6e8f0a1b"
        )
    );
    assert_eq!(
        shop.message_count,
        Some(9),
        "5 user and 4 assistant messages"
    );

    let filtered = env
        .index
        .sessions(&SessionsRequest {
            filters: Filters {
                session_id: Some(main.session_id.clone()),
                ..Filters::default()
            },
            ..SessionsRequest::default()
        })
        .unwrap();
    assert_eq!(filtered.len(), 1);
}

#[test]
fn read_pages_through_a_session() {
    let env = env();
    let mut req = ReadRequest::new("0190f3a2-1c00-7000-8000-00000000a001");
    req.limit = 4;
    let first = env.index.read(&req).unwrap();
    assert_eq!(first.records.len(), 4);
    assert_eq!(first.records[0].role, "user");
    let total = first.total.unwrap();
    assert!(total > 8);
    assert_eq!(first.next_offset, Some(4));
    req.offset = total - 1;
    let last = env.index.read(&req).unwrap();
    assert_eq!(last.records.len(), 1);
    assert_eq!(last.next_offset, None);
    let missing = env.index.read(&ReadRequest::new("nope")).unwrap();
    assert_eq!(missing.total, Some(0));
}

#[test]
fn appended_rewritten_and_deleted_transcripts_are_picked_up() {
    let env = env();
    assert!(search(&env.index, "zebrafish").is_empty());
    let main = pi_file(&env.home, "2026-03-01T09-00-00");

    // Appended: only the new line is parsed, and it is searchable.
    let line = r#"{"type":"message","id":"a1000099","parentId":"a1000020","timestamp":"2026-03-01T10:31:00.000Z","message":{"role":"user","content":[{"type":"text","text":"what about zebrafish"}],"timestamp":1772361060000}}"#;
    let mut text = std::fs::read_to_string(&main).unwrap();
    text.push_str(line);
    text.push('\n');
    std::fs::write(&main, &text).unwrap();
    let report = env.index.refresh(&Options::default()).unwrap().unwrap();
    assert_eq!(report.files_parsed, 1);
    assert!(report.bytes_parsed < line.len() as u64 + 2, "{report:?}");
    assert_eq!(search(&env.index, "zebrafish").len(), 1);

    // Unchanged: nothing to do.
    let report = env.index.refresh(&Options::default()).unwrap().unwrap();
    assert!(!report.changed(), "{report:?}");

    // Rewritten in place (same length or longer, different bytes before the offset).
    let rewritten = text.replacen("zebrafish", "starfish!", 1);
    std::fs::write(&main, format!("{rewritten}\n")).unwrap();
    env.index.refresh(&Options::default()).unwrap().unwrap();
    assert!(search(&env.index, "zebrafish").is_empty());
    assert_eq!(search(&env.index, "starfish").len(), 1);
    let pi_hits = search(&env.index, "回滚还来得及")
        .into_iter()
        .filter(|h| h.source == "pi")
        .count();
    assert_eq!(pi_hits, 1, "no duplicates after re-parse");

    // Deleted: its session goes away.
    std::fs::remove_file(&main).unwrap();
    env.index.refresh(&Options::default()).unwrap().unwrap();
    assert!(search(&env.index, "starfish").is_empty());
    assert_eq!(sessions_of(&env.index, "pi").len(), 2);
}

#[test]
fn rebuild_starts_from_scratch() {
    let env = env();
    let first = env.index.refresh(&Options::default()).unwrap().unwrap();
    assert_eq!(first.files_parsed, 11);
    let again = env
        .index
        .refresh(&Options {
            rebuild: true,
            ..Options::default()
        })
        .unwrap()
        .unwrap();
    assert!(again.rebuilt);
    assert_eq!(again.files_parsed, 11);
    assert!(!search(&env.index, "回滚").is_empty());
}

#[test]
fn compressing_a_rollout_keeps_its_records_without_parsing_again() {
    let env = env();
    let before = search(&env.index, "回滚");
    let hit = before
        .iter()
        .find(|h| h.source == "codex")
        .expect("codex hit")
        .clone();
    let plain = PathBuf::from(&hit.source_path);
    assert!(plain.extension().is_some_and(|e| e == "jsonl"));

    // What codex's compression worker does: write `x.jsonl.zst`, remove `x.jsonl`.
    let packed = PathBuf::from(format!("{}.zst", plain.display()));
    let bytes = std::fs::read(&plain).unwrap();
    std::fs::write(&packed, zstd::encode_all(&bytes[..], 3).unwrap()).unwrap();
    std::fs::remove_file(&plain).unwrap();

    let report = env.index.refresh(&Options::default()).unwrap().unwrap();
    assert_eq!(report.files_moved, 1, "{report:?}");
    assert_eq!(report.files_parsed, 0, "{report:?}");

    let after = search(&env.index, "回滚");
    let moved = after.iter().find(|h| h.source == "codex").unwrap();
    assert_eq!(
        moved.record_id, hit.record_id,
        "record ids survive compression"
    );
    assert_eq!(moved.source_path, packed.display().to_string());
    assert!(moved.snippet.contains("回滚"), "{}", moved.snippet);
    let page = env
        .index
        .context(&ContextRequest::new(&moved.record_id))
        .unwrap();
    assert!(
        page.records
            .iter()
            .any(|r| r.text.contains("在那之前回滚还来得及"))
    );
}

#[test]
fn codex_sessions_have_kinds_and_resume_commands() {
    let env = env();
    let codex = sessions_of(&env.index, "codex");
    let kinds: Vec<&str> = codex.iter().filter_map(|s| s.kind.as_deref()).collect();
    assert_eq!(codex.len(), 3, "{codex:#?}");
    for kind in ["main", "subagent", "exec"] {
        assert!(kinds.contains(&kind), "{kinds:?}");
    }
    let main = codex
        .iter()
        .find(|s| s.kind.as_deref() == Some("main"))
        .unwrap();
    let sub = codex
        .iter()
        .find(|s| s.kind.as_deref() == Some("subagent"))
        .unwrap();
    assert_eq!(
        main.resume_cmd.as_deref(),
        Some(
            format!(
                "cd /Users/alice/work/infra-notes && codex resume {}",
                main.session_id
            )
            .as_str()
        )
    );
    // A subagent thread reopens its parent.
    assert_eq!(sub.resume_cmd, main.resume_cmd);
}

#[test]
fn opencode_sessions_follow_the_database() {
    let env = env();
    let sessions = sessions_of(&env.index, "opencode");
    assert_eq!(sessions.len(), 3, "{sessions:#?}");
    let main = sessions
        .iter()
        .find(|s| s.session_id == "ses_8a1f0c2b3ffeQwErTyUiOpAsDf")
        .unwrap();
    assert_eq!(
        main.resume_cmd.as_deref(),
        Some("cd /Users/alice/work/blog && opencode --session ses_8a1f0c2b3ffeQwErTyUiOpAsDf")
    );

    // New content in the database is found after the next refresh, without re-reading the
    // other sessions.
    let db = env.home.join(".local/share/opencode/opencode.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
         SELECT 'prt_zzzz', message_id, session_id, time_created + 1, time_updated + 1,
                '{\"type\":\"text\",\"text\":\"zebrafish\"}'
         FROM part WHERE session_id = 'ses_8a1f0c2b3ffeQwErTyUiOpAsDf'
         ORDER BY id DESC LIMIT 1;",
    )
    .unwrap();
    drop(conn);
    let report = env.index.refresh(&Options::default()).unwrap().unwrap();
    assert_eq!(report.files_parsed, 1);
    assert_eq!(report.records_added as usize, {
        let page = env
            .index
            .read(&ReadRequest::new("ses_8a1f0c2b3ffeQwErTyUiOpAsDf"))
            .unwrap();
        page.total.unwrap() as usize
    });
    let hits = search(&env.index, "zebrafish");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].source, "opencode");
    assert!(hits[0].snippet.contains("zebrafish"));
}

#[test]
fn a_machine_without_transcripts_answers_with_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let index = NativeIndex::new(
        IndexPaths::new(dir.path().join("index")),
        Roots::standard(&home),
        Some(Duration::ZERO),
    );
    assert!(search(&index, "回滚").is_empty());
    let sessions = index.sessions(&SessionsRequest::default()).unwrap();
    assert!(sessions.is_empty());
}

#[test]
fn a_codex_session_split_over_files_reads_as_one() {
    let env = env();
    // Codex continues a long session in a new file with the same id and a `_<id>` suffix.
    let day = env.home.join(".codex/sessions/2026/03/14");
    let first = day.join("rollout-2026-03-14T09-30-05-019cf1a2-7b3c-7d10-9e21-4a5b6c7d8e9f.jsonl");
    std::fs::copy(
        &first,
        day.join(
            "rollout-2026-03-14T09-30-05-019cf1a2-7b3c-7d10-9e21-4a5b6c7d8e9f_019cf1a2-0000-7000-8000-000000000000.jsonl",
        ),
    )
    .unwrap();
    env.index.refresh(&Options::default()).unwrap();
    let sid = "019cf1a2-7b3c-7d10-9e21-4a5b6c7d8e9f";

    let files: Vec<_> = sessions_of(&env.index, "codex")
        .into_iter()
        .filter(|s| s.session_id == sid)
        .collect();
    assert_eq!(files.len(), 1, "one row for both files");
    let single = files[0].message_count.unwrap();

    let mut req = SearchRequest::new("回滚");
    req.unique_sessions = true;
    let hits = env.index.search(&req).unwrap();
    assert_eq!(hits.iter().filter(|h| h.session_id == sid).count(), 1);

    let mut read = ReadRequest::new(sid);
    read.limit = 500;
    read.max_chars = 200_000;
    let page = env.index.read(&read).unwrap();
    let total = page.total.unwrap() as usize;
    assert_eq!(page.records.len(), total);
    assert!(
        single > 0 && total.is_multiple_of(2),
        "both files, one after the other"
    );
    let half = total / 2;
    assert_eq!(page.records[0].text, page.records[half].text);
    assert_ne!(page.records[0].record_id, page.records[half].record_id);
    // Paging crosses from one file into the next.
    read.offset = half as u32 - 1;
    read.limit = 2;
    let page = env.index.read(&read).unwrap();
    assert_eq!(page.records.len(), 2);
    assert_eq!(page.next_offset, Some(half as u32 + 1));
}
