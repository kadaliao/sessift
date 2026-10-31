//! End-to-end tests: the real binary, with "remote" machines started as local commands
//! (`sessift serve --stdio` over pipes instead of SSH) and fixture data per machine.

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const T0: i64 = 1_793_000_000_000;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sessift")
}

fn record(i: i64, role: &str, text: &str) -> Value {
    json!({"record_id": format!("r{i}"), "ts_ms": T0 + i * 1000, "role": role,
           "text": text, "total_chars": text.chars().count(), "truncated": false})
}

fn session(id: &str, source: &str, last: i64, title: &str) -> Value {
    json!({"source": source, "session_id": id, "source_path": format!("/data/{id}.jsonl"),
           "project": "sessift", "cwd": "/w/sessift", "title": title,
           "started_ms": T0, "last_ms": T0 + last, "message_count": 3})
}

fn fixtures() -> [(&'static str, Value); 3] {
    let many: Vec<Value> = (0..120)
        .map(|i| {
            record(
                i,
                if i % 2 == 0 { "user" } else { "assistant" },
                &format!("第 {i} 条记录"),
            )
        })
        .collect();
    [
        (
            "local",
            json!({
                "sessions": [session("loc-1", "claude", 3000, "本机会话")],
                "records": {"loc-1": [
                    record(1, "user", "how do we deploy the broker"),
                    record(2, "assistant", "在那之前回滚还来得及，部署 broker 需要先 install"),
                ]}
            }),
        ),
        (
            "a",
            json!({
                "sessions": [session("a-1", "codex", 2000, "machine a")],
                "records": {"a-1": [record(3, "assistant", "deploy deploy deploy to production")]}
            }),
        ),
        (
            "b",
            json!({
                "sessions": [session("b-1", "pi", 1000, "long one"), session("b-2", "claude", 4000, "newest")],
                "records": {"b-1": many, "b-2": [record(9, "user", "deploy twice: deploy")]}
            }),
        ),
    ]
}

struct TestEnv {
    dir: TempDir,
}

impl TestEnv {
    /// Machines `a` and `b` are fixture servers; `extra` adds more `[[machine]]` tables.
    fn new(extra: &str) -> Self {
        let dir = tempfile::Builder::new().prefix("sft").tempdir().unwrap();
        for (name, data) in fixtures() {
            std::fs::write(dir.path().join(format!("{name}.json")), data.to_string()).unwrap();
        }
        let env = Self { dir };
        let config = format!(
            r#"
[defaults]
machines = ["local", "a", "b"]
timeout = "4s"

[backend]
kind = "fixture"
fixture = {local}

[broker]
idle_timeout = "20s"
probe_after_idle = "300ms"

{a}
{b}
{extra}
"#,
            local = q(env.path("local.json")),
            a = env.machine("a", &[]),
            b = env.machine("b", &[]),
        );
        std::fs::write(env.home().join("config.toml"), config).unwrap();
        env
    }

    /// Append a `[[machine]]` table to the config.
    fn add(&self, table: &str) {
        let path = self.home().join("config.toml");
        let config = std::fs::read_to_string(&path).unwrap() + table;
        std::fs::write(path, config).unwrap();
    }

    fn home(&self) -> &Path {
        self.dir.path()
    }

    fn path(&self, name: &str) -> String {
        self.home().join(name).to_string_lossy().into_owned()
    }

    /// A `[[machine]]` table serving `<id>.json` (or `b.json` for unknown ids) with extra env.
    fn machine(&self, id: &str, env: &[&str]) -> String {
        let data = match id {
            "a" | "b" => format!("{id}.json"),
            _ => "b.json".to_string(),
        };
        let mut argv = vec![
            "env".to_string(),
            format!("SESSIFT_FIXTURE={}", self.path(&data)),
        ];
        argv.extend(env.iter().map(|s| s.to_string()));
        argv.extend([bin().to_string(), "serve".into(), "--stdio".into()]);
        let argv: Vec<String> = argv.iter().map(|s| q(s.clone())).collect();
        format!(
            "[[machine]]\nid = \"{id}\"\ncommand = [{}]\n",
            argv.join(", ")
        )
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(bin());
        cmd.args(args)
            .env("SESSIFT_HOME", self.home())
            .env("NO_COLOR", "1")
            .env_remove("SESSIFT_FIXTURE")
            .env_remove("SESSIFT_FIXTURE_DELAY_MS")
            .env_remove("SESSIFT_TEST_NOISE");
        // Running the tests from an agent must not hide its session from them.
        for var in [
            "CLAUDE_CODE_SESSION_ID",
            "CODEX_THREAD_ID",
            "PI_SESSION_ID",
            "GROK_SESSION_ID",
        ] {
            cmd.env_remove(var);
        }
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = args.to_vec();
        all.extend(["--format", "json"]);
        let out = self.run(&all);
        assert!(
            out.status.success(),
            "sessift {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = self.run(&["broker", "stop"]);
    }
}

fn q(s: impl Into<String>) -> String {
    toml_quote(&s.into())
}

fn toml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn ids(results: &Value, key: &str) -> Vec<String> {
    results["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "{}:{}",
                r["machine"].as_str().unwrap(),
                r[key].as_str().unwrap()
            )
        })
        .collect()
}

#[test]
fn search_merges_every_machine_by_score() {
    let env = TestEnv::new("");
    for mode in [&["--no-broker"][..], &[][..]] {
        let mut args = vec!["search", "deploy"];
        args.extend(mode);
        let out = env.json(&args);
        assert_eq!(
            ids(&out, "session_id"),
            vec!["a:a-1", "b:b-2", "local:loc-1"],
            "mode {mode:?}"
        );
        let machines = out["machines"].as_array().unwrap();
        assert_eq!(machines.len(), 3);
        assert!(machines.iter().all(|m| m["ok"] == true), "{machines:?}");
        let via = if mode.is_empty() { "broker" } else { "direct" };
        assert_eq!(machines[1]["via"], via);
    }
}

#[test]
fn chinese_substrings_match() {
    let env = TestEnv::new("");
    let out = env.json(&["search", "回滚", "-m", "local"]);
    assert_eq!(ids(&out, "session_id"), vec!["local:loc-1"]);
    assert!(
        out["results"][0]["snippet"]
            .as_str()
            .unwrap()
            .contains("回滚")
    );
}

#[test]
fn pipes_get_jsonl_and_filters_apply() {
    let env = TestEnv::new("");
    let out = env.run(&["search", "deploy", "--source", "claude"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let lines: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let sessions: Vec<&str> = lines
        .iter()
        .map(|l| l["session_id"].as_str().unwrap())
        .collect();
    assert_eq!(sessions, vec!["b-2", "loc-1"]);
}

#[test]
fn sessions_merge_by_recency_and_page() {
    let env = TestEnv::new("");
    let out = env.json(&["sessions"]);
    assert_eq!(
        ids(&out, "session_id"),
        vec!["b:b-2", "local:loc-1", "a:a-1", "b:b-1"]
    );
    let page = env.json(&["sessions", "--offset", "1", "--limit", "2"]);
    assert_eq!(ids(&page, "session_id"), vec!["local:loc-1", "a:a-1"]);
    let one = env.json(&["sessions", "-m", "b", "--offset", "1"]);
    assert_eq!(ids(&one, "session_id"), vec!["b:b-1"]);
}

#[test]
fn the_calling_agents_sessions_are_left_out() {
    let env = TestEnv::new("");
    let out = env
        .cmd(&["sessions", "--format", "json"])
        .env("CLAUDE_CODE_SESSION_ID", "loc-1")
        .env("CODEX_THREAD_ID", "b-2")
        .output()
        .unwrap();
    let out: Value = serde_json::from_slice(&out.stdout).unwrap();
    // Remote machines get the exclusion with the request.
    assert_eq!(ids(&out, "session_id"), vec!["a:a-1", "b:b-1"]);

    let out = env
        .cmd(&["search", "deploy", "--include-current", "--format", "json"])
        .env("CLAUDE_CODE_SESSION_ID", "loc-1")
        .output()
        .unwrap();
    let out: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(ids(&out, "session_id").contains(&"local:loc-1".to_string()));

    // Asked for by id, the caller's own session is shown.
    let out = env
        .cmd(&["sessions", "--session", "loc-1", "--format", "json"])
        .env("CLAUDE_CODE_SESSION_ID", "loc-1")
        .output()
        .unwrap();
    let out: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(ids(&out, "session_id"), vec!["local:loc-1"]);
}

#[test]
fn read_around_a_search_hit() {
    let env = TestEnv::new("");
    // Only b has the record; the others answer with errors, which are not worth a warning.
    let out = env.run(&[
        "read", "b-1", "--around", "r60", "-n", "8", "--format", "json",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stderr(&out), "");
    let page: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(page["machine"], "b");
    assert_eq!(page["offset"], 58, "a quarter of the page before the hit");
    let records = page["records"].as_array().unwrap();
    assert_eq!(records.len(), 8);
    assert_eq!(records[2]["record_id"], "r60");
    // Filled in by the client when the server leaves it out.
    assert_eq!(page["next_offset"], 66);

    let missing = env.run(&["read", "b-1", "--around", "r999"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(stderr(&missing).contains("r999"), "{}", stderr(&missing));
    let both = env.run(&["read", "b-1", "--around", "r1", "--offset", "3"]);
    assert_eq!(
        both.status.code(),
        Some(2),
        "--around and --offset conflict"
    );
}

#[test]
fn read_the_end_of_a_session() {
    let env = TestEnv::new("");
    let page = env.json(&["read", "b-1", "--tail", "5"]);
    assert_eq!(page["machine"], "b");
    assert_eq!(page["offset"], 115);
    assert_eq!(page["records"].as_array().unwrap().len(), 5);
    assert_eq!(page["records"][4]["text"], "第 119 条记录");
    assert_eq!(page["next_offset"], Value::Null);
    let missing = env.run(&["read", "nope", "--tail", "5"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(stderr(&missing).contains("not found"));
}

#[test]
fn hits_inside_one_session_come_in_time_order() {
    let env = TestEnv::new("");
    let out = env.json(&["search", "broker", "--session", "loc-1"]);
    let records: Vec<_> = out["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["record_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(records, ["r1", "r2"]);
}

#[test]
fn an_empty_search_says_so() {
    let env = TestEnv::new("");
    let out = env.run(&["search", "nothing-like-this", "--format", "jsonl"]);
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    assert!(
        stderr(&out).contains("no matches on local, a, b"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn read_finds_the_machine_and_pages() {
    let env = TestEnv::new("");
    let page = env.json(&["read", "b-1", "--limit", "50"]);
    assert_eq!(page["machine"], "b");
    assert_eq!(page["total"], 120);
    assert_eq!(page["next_offset"], 50);
    assert_eq!(page["records"].as_array().unwrap().len(), 50);
    let last = env.json(&["read", "b-1", "-m", "b", "--offset", "100", "--limit", "50"]);
    assert_eq!(last["records"].as_array().unwrap().len(), 20);
    assert_eq!(last["next_offset"], Value::Null);

    let text = env.run(&["read", "b-1", "--format", "text", "--limit", "2"]);
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("第 0 条记录"), "{text}");
    assert!(
        text.contains("next: sessift read b-1 -m b --offset 2 -n 2 --format text"),
        "{text}"
    );

    let missing = env.run(&["read", "nope"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(stderr(&missing).contains("not found on local, a, b"));
}

#[test]
fn page_budget_truncates_long_records() {
    let env = TestEnv::new("");
    let page = env.json(&[
        "read",
        "b-1",
        "-m",
        "b",
        "--limit",
        "100",
        "--max-chars",
        "300",
    ]);
    let total: usize = page["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["text"].as_str().unwrap().chars().count())
        .sum();
    assert!(total <= 300, "{total}");
    // The page stops where the budget runs out; the rest is on the next page.
    let records = page["records"].as_array().unwrap();
    assert!(records.len() < 100);
    assert_eq!(records.last().unwrap()["truncated"], true);
    assert_eq!(page["next_offset"], records.len());
    let next = env.json(&[
        "read",
        "b-1",
        "-m",
        "b",
        "--offset",
        &records.len().to_string(),
        "--max-chars",
        "300",
    ]);
    assert_eq!(next["offset"], records.len());
    assert!(!next["records"][0]["text"].as_str().unwrap().is_empty());
}

#[test]
fn a_slow_machine_does_not_hold_up_the_others() {
    let env = TestEnv::new("");
    env.add(&env.machine("slow", &["SESSIFT_FIXTURE_DELAY_MS=5000"]));

    for mode in [&["--no-broker"][..], &[][..]] {
        let started = Instant::now();
        let mut args = vec![
            "search",
            "deploy",
            "-m",
            "all",
            "--timeout",
            "700ms",
            "--format",
            "json",
        ];
        args.extend(mode);
        let out = env.run(&args);
        let elapsed = started.elapsed();
        assert!(out.status.success(), "{}", stderr(&out));
        assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["results"].as_array().unwrap().len(), 3);
        let slow = value["machines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == "slow")
            .unwrap()
            .clone();
        assert_eq!(slow["ok"], false);
        assert_eq!(slow["error"]["code"], "timeout");
        assert!(stderr(&out).contains("warning: slow:"), "{}", stderr(&out));
    }
}

#[test]
fn all_machines_failing_is_an_error() {
    let env = TestEnv::new("");
    let out = env.run(&["search", "__fail__", "-m", "a", "-m", "local"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("a: injected failure"), "{err}");
    assert!(err.contains("local: injected failure"), "{err}");
}

#[test]
fn shell_noise_before_the_server_is_skipped() {
    let env = TestEnv::new("");
    env.add(&env.machine("noisy", &["SESSIFT_TEST_NOISE=1"]));
    let out = env.json(&["search", "deploy", "-m", "noisy", "--no-broker"]);
    assert_eq!(ids(&out, "session_id"), vec!["noisy:b-2"]);
}

#[test]
fn a_missing_remote_binary_gets_an_install_hint() {
    let env = TestEnv::new(
        r#"[[machine]]
id = "bare"
command = ["sh", "-c", "echo 'fish: Unknown command: sessift' >&2; exit 127"]
"#,
    );
    for mode in [&["--no-broker"][..], &[][..]] {
        let mut args = vec!["sessions", "-m", "bare", "-m", "a"];
        args.extend(mode);
        let out = env.run(&args);
        assert!(out.status.success(), "{}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains("bare: sessift is not installed"), "{err}");
        assert!(err.contains("sessift remote install bare"), "{err}");
    }
}

#[test]
fn broker_is_shared_restartable_and_reports_status() {
    let env = TestEnv::new("");
    assert!(env.json(&["broker", "status"])["running"] == false);
    let first = env.json(&["sessions"]);
    assert_eq!(first["machines"][1]["via"], "broker");

    // Several CLI processes at once share the one broker.
    let children: Vec<_> = (0..6)
        .map(|_| {
            env.cmd(&["search", "deploy", "--format", "json"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["results"].as_array().unwrap().len(), 3);
    }

    let status = env.json(&["broker", "status"]);
    assert_eq!(status["running"], true);
    let pid = status["pid"].as_u64().unwrap();
    // 2 remote machines x (1 sessions + 6 searches)
    assert_eq!(status["requests"], 14);
    let machines = status["machines"].as_array().unwrap();
    assert!(
        machines.iter().all(|m| m["connected"] == true),
        "{machines:?}"
    );

    let stop = env.run(&["broker", "stop"]);
    assert!(String::from_utf8_lossy(&stop.stdout).contains("stopped broker"));
    assert!(env.json(&["broker", "status"])["running"] == false);
    env.json(&["search", "deploy"]);
    let restarted = env.json(&["broker", "status"]);
    assert_ne!(restarted["pid"].as_u64().unwrap(), pid);
}

#[test]
fn machines_lists_versions_and_failures() {
    let env = TestEnv::new(
        r#"[[machine]]
id = "dead"
command = ["false"]
"#,
    );
    let out = env.run(&["machines", "--format", "json", "--no-broker"]);
    assert_eq!(out.status.code(), Some(1));
    let list: Value = serde_json::from_slice(&out.stdout).unwrap();
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 4);
    for row in &list[..3] {
        assert_eq!(row["ok"], true, "{row}");
        assert_eq!(row["info"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(row["info"]["backend"]["kind"], "fixture");
    }
    assert_eq!(list[3]["id"], "dead");
    assert_eq!(list[3]["ok"], false);
}

#[test]
fn init_imports_memex_machines() {
    let dir = tempfile::tempdir().unwrap();
    let memex = dir.path().join("memex.toml");
    std::fs::write(
        &memex,
        "[multi_machine]\ndefault = [\"local\", \"mm\"]\n[[machines]]\nid = \"mm\"\nlabel = \"Mac mini\"\n[machines.control]\ntype = \"ssh\"\nhost = \"mm\"\n",
    )
    .unwrap();
    let home = dir.path().join("home");
    let out = Command::new(bin())
        .args(["init", "--from-memex"])
        .arg(&memex)
        .env("SESSIFT_HOME", &home)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(
        config.contains("machines = [\"local\", \"mm\"]"),
        "{config}"
    );
    assert!(config.contains("ssh = \"mm\""), "{config}");
    let again = Command::new(bin())
        .arg("init")
        .env("SESSIFT_HOME", &home)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&again.stdout).contains("already exists"));
}

#[test]
fn bad_arguments_fail_before_any_query() {
    let env = TestEnv::new("");
    let out = env.run(&["search", "x", "-m", "ghost"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("unknown machine 'ghost'"));
    let out = env.run(&["search", "x", "--since", "yesterday"]);
    assert!(stderr(&out).contains("invalid time 'yesterday'"));
    let out = env.run(&["sessions", "--offset", "490", "--limit", "20"]);
    assert!(stderr(&out).contains("pick one machine"));
}

/// A machine whose every connection runs a fresh server and records its pid, so a test can
/// freeze or kill the server behind the broker's back.
fn recording_machine(env: &TestEnv, id: &str, extra_env: &str) -> String {
    let script = format!(
        "exec 3<&0; SESSIFT_FIXTURE='{}' {extra_env} '{}' serve --stdio <&3 & echo $! >> '{}'; wait",
        env.path("b.json"),
        bin(),
        env.path(&format!("{id}.pids")),
    );
    format!(
        "[[machine]]\nid = \"{id}\"\ncommand = [\"sh\", \"-c\", {}]\n",
        toml_quote(&script)
    )
}

fn pids(env: &TestEnv, id: &str) -> Vec<String> {
    std::fs::read_to_string(env.home().join(format!("{id}.pids")))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Kills every server a recording machine started, even when the test fails part-way.
struct KillRecorded<'a>(&'a TestEnv, &'a str);

impl Drop for KillRecorded<'_> {
    fn drop(&mut self) {
        for pid in pids(self.0, self.1) {
            signal("-KILL", &pid);
        }
    }
}

fn signal(sig: &str, pid: &str) {
    Command::new("kill")
        .args([sig, pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
}

#[test]
fn a_frozen_connection_is_detected_and_replaced() {
    let env = TestEnv::new("");
    env.add(&recording_machine(&env, "frozen", ""));
    let _cleanup = KillRecorded(&env, "frozen");
    env.json(&["search", "deploy", "-m", "frozen"]);
    let first = pids(&env, "frozen");
    assert_eq!(first.len(), 1);
    // The server stops answering but its pipes stay open, like a peer behind a dead network.
    signal("-STOP", &first[0]);
    std::thread::sleep(Duration::from_millis(400));

    let started = Instant::now();
    let out = env.json(&["search", "deploy", "-m", "frozen", "--timeout", "4s"]);
    let elapsed = started.elapsed();
    assert_eq!(ids(&out, "session_id"), vec!["frozen:b-2"]);
    assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    assert_eq!(pids(&env, "frozen").len(), 2, "the broker reconnected once");
}

#[test]
fn a_server_lost_mid_request_is_retried_on_a_new_connection() {
    let env = TestEnv::new("");
    env.add(&recording_machine(
        &env,
        "flaky",
        "SESSIFT_FIXTURE_DELAY_MS=800",
    ));
    env.json(&["sessions", "-m", "flaky"]);
    let first = pids(&env, "flaky");
    let request = env
        .cmd(&[
            "search",
            "deploy",
            "-m",
            "flaky",
            "--format",
            "json",
            "--timeout",
            "5s",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    signal("-TERM", &first[0]);
    let out = request.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["results"].as_array().unwrap().len(), 1, "{value}");
    assert_eq!(pids(&env, "flaky").len(), 2);
}

#[test]
fn a_remote_that_cannot_start_is_not_reported_as_missing() {
    let env = TestEnv::new(
        r#"[[machine]]
id = "oldlib"
command = ["sh", "-c", "echo 'sessift: error while loading shared libraries: libssl.so.3: cannot open shared object file: No such file or directory' >&2; exit 127"]
"#,
    );
    let out = env.run(&["sessions", "-m", "oldlib", "-m", "a", "--no-broker"]);
    let err = stderr(&out);
    assert!(
        err.contains("oldlib: the remote sessift cannot start"),
        "{err}"
    );
    assert!(!err.contains("remote install"), "{err}");
}

fn wait_for<T>(mut probe: impl FnMut() -> Option<T>, what: &str) -> T {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_broker_from_another_build_retires_without_dropping_requests() {
    let env = TestEnv::new("");
    env.add(&env.machine("slow", &["SESSIFT_FIXTURE_DELAY_MS=1500"]));
    // A copy at another path has a different build id.
    let other = env.home().join("sessift-other");
    std::fs::copy(bin(), &other).unwrap();
    let slow = Command::new(&other)
        .args(["search", "deploy", "-m", "slow", "--format", "json"])
        .env("SESSIFT_HOME", env.home())
        .env("NO_COLOR", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let old = wait_for(
        || {
            let status = env.json(&["broker", "status"]);
            (status["running"] == true && status["clients"].as_u64() >= Some(1)).then_some(status)
        },
        "the other build's broker",
    );
    let old_pid = old["pid"].as_u64().unwrap();

    // This build uses the old broker for now and asks it to retire.
    let out = env.json(&["search", "deploy", "-m", "a"]);
    assert_eq!(out["machines"][0]["via"], "broker");
    assert_eq!(out["machines"][0]["ok"], true);
    let status = env.json(&["broker", "status"]);
    assert_eq!(status["pid"].as_u64(), Some(old_pid));
    assert_eq!(status["retiring"], true);

    // The other build's slow request is not interrupted.
    let slow = slow.wait_with_output().unwrap();
    assert!(slow.status.success(), "{}", stderr(&slow));
    let value: Value = serde_json::from_slice(&slow.stdout).unwrap();
    assert_eq!(value["results"].as_array().unwrap().len(), 1, "{value}");

    // Once idle it exits, and the next query starts this build's broker.
    wait_for(
        || (env.json(&["broker", "status"])["running"] == false).then_some(()),
        "the retired broker to exit",
    );
    env.json(&["search", "deploy", "-m", "a"]);
    let fresh = env.json(&["broker", "status"]);
    assert_ne!(fresh["pid"].as_u64(), Some(old_pid));
    assert_eq!(fresh["retiring"], false);
}

#[test]
fn shell_snippets_are_valid_syntax() {
    for (shell, check) in [
        ("fish", vec!["fish", "--no-execute"]),
        ("zsh", vec!["zsh", "-n"]),
        ("bash", vec!["bash", "-n"]),
    ] {
        if Command::new(check[0]).arg("--version").output().is_err() {
            continue;
        }
        let out = Command::new(bin()).args(["shell", shell]).output().unwrap();
        assert!(out.status.success());
        let path =
            std::env::temp_dir().join(format!("sessift-snippet-{}.{shell}", std::process::id()));
        std::fs::write(&path, &out.stdout).unwrap();
        let checked = Command::new(check[0])
            .args(&check[1..])
            .arg(&path)
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(
            checked.status.success(),
            "{shell}: {}",
            String::from_utf8_lossy(&checked.stderr)
        );
    }
    let out = Command::new(bin())
        .args(["shell", "fish", "--key", "ctrl-o"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("bind ctrl-o sessift-widget"));
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// A home directory with the synthetic transcripts of every source, and a sessift home
/// configured for the native backend.
fn native_machine(root: &Path, name: &str) -> (String, String) {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let home = root.join(format!("{name}-home"));
    copy_tree(&fixtures.join("home"), &home);
    let opencode = home.join(".local/share/opencode");
    std::fs::create_dir_all(&opencode).unwrap();
    let conn = rusqlite::Connection::open(opencode.join("opencode.db")).unwrap();
    for file in ["schema.sql", "insert.sql"] {
        let sql = std::fs::read_to_string(fixtures.join("opencode").join(file)).unwrap();
        conn.execute_batch(&sql).unwrap();
    }
    let sessift_home = root.join(format!("{name}-sessift"));
    std::fs::create_dir_all(&sessift_home).unwrap();
    (
        home.to_string_lossy().into_owned(),
        sessift_home.to_string_lossy().into_owned(),
    )
}

#[test]
fn native_backend_end_to_end() {
    let root = tempfile::Builder::new().prefix("sft").tempdir().unwrap();
    let (home, sessift_home) = native_machine(root.path(), "local");
    let (far_home, far_sessift) = native_machine(root.path(), "far");
    let native = "[backend]\nkind = \"native\"\n";
    std::fs::write(Path::new(&far_sessift).join("config.toml"), native).unwrap();
    let far_argv = [
        "env".to_string(),
        format!("HOME={far_home}"),
        format!("SESSIFT_HOME={far_sessift}"),
        bin().to_string(),
        "serve".into(),
        "--stdio".into(),
    ]
    .map(q)
    .join(", ");
    std::fs::write(
        Path::new(&sessift_home).join("config.toml"),
        format!(
            "[defaults]\nmachines = [\"local\", \"far\"]\ntimeout = \"10s\"\n\n{native}\n[[machine]]\nid = \"far\"\ncommand = [{far_argv}]\n"
        ),
    )
    .unwrap();
    let run = |args: &[&str]| -> Value {
        let out = Command::new(bin())
            .args(args)
            .args(["--format", "json", "--no-broker"])
            .env("HOME", &home)
            .env("SESSIFT_HOME", &sessift_home)
            .env_remove("SESSIFT_FIXTURE")
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    };

    // The first query builds each machine's index (small enough to do inline).
    let found = run(&["search", "回滚", "-n", "50"]);
    let mut seen: Vec<String> = found["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["snippet"].as_str().unwrap().contains("回滚还来得及"))
        .map(|r| {
            format!(
                "{}:{}",
                r["machine"].as_str().unwrap(),
                r["source"].as_str().unwrap()
            )
        })
        .collect();
    seen.sort();
    seen.dedup();
    assert_eq!(
        seen,
        [
            "far:claude",
            "far:codex",
            "far:grok",
            "far:opencode",
            "far:pi",
            "local:claude",
            "local:codex",
            "local:grok",
            "local:opencode",
            "local:pi"
        ]
    );

    // Lists show conversations unless asked for every kind.
    let sessions = run(&["sessions", "-n", "100"]);
    let rows = sessions["results"].as_array().unwrap();
    assert_eq!(rows.len(), 2 * 10, "10 conversations per machine");
    assert!(rows.iter().all(|r| r["kind"] != "subagent"));
    let sessions = run(&["sessions", "-n", "100", "--kind", "all"]);
    let rows = sessions["results"].as_array().unwrap();
    assert_eq!(rows.len(), 2 * 16, "16 sessions per machine");
    let codex = rows
        .iter()
        .find(|r| r["machine"] == "far" && r["source"] == "codex" && r["kind"] == "main")
        .unwrap();

    let page = run(&[
        "read",
        codex["session_id"].as_str().unwrap(),
        "-m",
        "far",
        "-n",
        "3",
    ]);
    assert_eq!(page["machine"], "far");
    assert_eq!(page["records"].as_array().unwrap().len(), 3);
    assert!(page["total"].as_u64().unwrap() > 3);

    let stats = run(&["index", "stats"]);
    assert_eq!(stats["pending_files"], 0);
    let sources: Vec<&str> = stats["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["source"].as_str().unwrap())
        .collect();
    assert_eq!(sources, ["claude", "codex", "grok", "opencode", "pi"]);
}
