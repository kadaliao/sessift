//! `sessift pick --herdr`: hand the picked session to Herdr instead of a shell binding.
//!
//! The Herdr plugin (`herdr/` in this repository) opens the picker in a popup. A popup has no
//! pane of its own; the tiled pane under it comes from `HERDR_PLUGIN_CONTEXT_JSON`. Everything
//! here goes through the Herdr CLI (`HERDR_BIN_PATH`), which prints JSON on stdout.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::config::LOCAL;
use crate::pick::{self, Chosen};
use crate::text;

/// Shells whose prompt can take a typed command.
const SHELLS: &[&str] = &[
    "fish", "zsh", "bash", "sh", "dash", "ksh", "nu", "elvish", "xonsh",
];

/// Tab labels are cut to this many columns.
const LABEL_WIDTH: usize = 24;

pub struct Herdr {
    bin: String,
    workspace: Option<String>,
    /// The tiled pane the popup was opened over.
    origin: Option<String>,
    copier: fn(&str) -> Result<()>,
}

/// Where the session ended up.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Already running in this pane; focused it.
    Focused(String),
    /// Resumed in the idle shell under the popup.
    InPlace(String),
    /// Resumed in a new tab, in this pane.
    NewTab(String),
    /// Put the command on the prompt of this pane.
    Filled(String),
    /// Copied the command to the clipboard.
    Copied,
}

impl Herdr {
    pub fn from_env() -> Self {
        let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
        let context: Value = env("HERDR_PLUGIN_CONTEXT_JSON")
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or(Value::Null);
        let field = |key: &str| context.get(key).and_then(Value::as_str).map(str::to_string);
        Self {
            bin: env("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into()),
            workspace: field("workspace_id").or_else(|| env("HERDR_WORKSPACE_ID")),
            origin: field("focused_pane_id").or_else(|| env("HERDR_PANE_ID")),
            copier: pick::copy,
        }
    }

    /// Enter: focus the pane already running the session, else resume it in the idle shell under
    /// the popup, else in a new tab.
    pub fn resume(&self, chosen: &Chosen) -> Result<Outcome> {
        if chosen.machine == LOCAL
            && let Some(pane) = self.pane_running(&chosen.session_id)?
        {
            self.call(&["agent", "focus", &pane])?;
            self.notify("会话已经开着，已跳过去", chosen.title.as_deref());
            return Ok(Outcome::Focused(pane));
        }
        if let Some(origin) = self.idle_origin() {
            self.call(&["pane", "run", &origin, &chosen.command])?;
            return Ok(Outcome::InPlace(origin));
        }
        let label = tab_label(chosen);
        let mut args = vec!["tab", "create", "--label", &label, "--focus"];
        if let Some(workspace) = &self.workspace {
            args.extend(["--workspace", workspace]);
        }
        // The tab's shell stays behind when the agent exits; start it where the session lives.
        if chosen.machine == LOCAL
            && let Some(cwd) = chosen.cwd.as_deref().filter(|cwd| Path::new(cwd).is_dir())
        {
            args.extend(["--cwd", cwd]);
        }
        let created = self.call(&args)?;
        let pane = created
            .pointer("/root_pane/pane_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("herdr tab create returned no pane"))?
            .to_string();
        self.call(&["pane", "run", &pane, &chosen.command])?;
        Ok(Outcome::NewTab(pane))
    }

    /// `e`: put the command on the prompt under the popup, or copy it when there is no prompt.
    pub fn edit(&self, chosen: &Chosen) -> Result<Outcome> {
        if let Some(origin) = self.idle_origin() {
            self.call(&["pane", "send-text", &origin, &chosen.command])?;
            return Ok(Outcome::Filled(origin));
        }
        (self.copier)(&chosen.command)?;
        self.notify("已复制恢复命令", Some(&chosen.command));
        Ok(Outcome::Copied)
    }

    /// Report a failure where the user can see it: the popup closes when sessift exits.
    pub fn fail(&self, chosen: &Chosen, err: &anyhow::Error) {
        let copied = (self.copier)(&chosen.command).is_ok();
        let title = if copied {
            "sessift 没能在 herdr 里打开会话，恢复命令已复制"
        } else {
            "sessift 没能在 herdr 里打开会话"
        };
        self.notify(title, Some(&format!("{err:#}")));
    }

    /// The pane whose agent Herdr reports as running this session.
    fn pane_running(&self, session_id: &str) -> Result<Option<String>> {
        if session_id.is_empty() {
            return Ok(None);
        }
        let list = self.call(&["pane", "list"])?;
        let panes = list.get("panes").and_then(Value::as_array);
        Ok(panes
            .into_iter()
            .flatten()
            .find(|pane| {
                pane.pointer("/agent_session/value").and_then(Value::as_str) == Some(session_id)
            })
            .and_then(|pane| pane.get("pane_id")?.as_str())
            .map(str::to_string))
    }

    /// The pane under the popup, when its own shell is in the foreground at a prompt.
    fn idle_origin(&self) -> Option<String> {
        let origin = self.origin.as_deref()?;
        let info = self
            .call(&["pane", "process-info", "--pane", origin])
            .ok()?;
        let info = info.get("process_info")?;
        let shell = info.get("shell_pid")?.as_i64()?;
        if info.get("foreground_process_group_id")?.as_i64()? != shell {
            return None;
        }
        let foreground = info.get("foreground_processes")?.as_array()?;
        let is_shell = foreground.iter().any(|process| {
            process.get("pid").and_then(Value::as_i64) == Some(shell)
                && process
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| SHELLS.contains(&name.trim_start_matches('-')))
        });
        is_shell.then(|| origin.to_string())
    }

    fn notify(&self, title: &str, body: Option<&str>) {
        let mut args = vec!["notification", "show", title];
        if let Some(body) = body {
            args.extend(["--body", body]);
        }
        let _ = self.call(&args);
    }

    /// Run `herdr <args>` and return the `result` of its JSON reply.
    fn call(&self, args: &[&str]) -> Result<Value> {
        let what = args.iter().take(2).copied().collect::<Vec<_>>().join(" ");
        let output = Command::new(&self.bin)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("run {} {what}", self.bin))?;
        let reply: Option<Value> = serde_json::from_slice(&output.stdout).ok();
        if let Some(message) = reply
            .as_ref()
            .and_then(|r| r.pointer("/error/message"))
            .and_then(Value::as_str)
        {
            bail!("herdr {what}: {message}");
        }
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "herdr {what} failed: {}",
                text::sanitize_line(stderr.trim())
            );
        }
        Ok(reply
            .and_then(|mut r| r.get_mut("result").map(Value::take))
            .unwrap_or(Value::Null))
    }
}

/// The title, else the directory name; remote sessions get their machine in front.
fn tab_label(chosen: &Chosen) -> String {
    let title = chosen
        .title
        .as_deref()
        .map(|t| text::collapse_ws(&text::sanitize_line(t)))
        .filter(|t| !t.is_empty());
    let dir = || {
        chosen
            .cwd
            .as_deref()
            .and_then(|cwd| cwd.trim_end_matches('/').rsplit('/').next())
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    };
    let name = title
        .or_else(dir)
        .unwrap_or_else(|| chosen.session_id.chars().take(8).collect());
    let name = if chosen.machine == LOCAL {
        name
    } else {
        format!("{}·{name}", chosen.machine)
    };
    text::fit_width(&name, LABEL_WIDTH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fake `herdr` that logs each call and answers from files next to it.
    struct Fake {
        dir: tempfile::TempDir,
    }

    impl Fake {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script = r#"#!/bin/sh
dir=$(dirname "$0")
printf '%s|' "$@" >> "$dir/calls"; echo >> "$dir/calls"
if [ -f "$dir/fail" ] && [ "$1 $2" = "$(cat "$dir/fail")" ]; then
  echo '{"error":{"code":"boom","message":"it broke"},"id":"x"}'; exit 1
fi
case "$1 $2" in
  "pane list") cat "$dir/panes.json" ;;
  "pane process-info") cat "$dir/proc.json" ;;
  "tab create") echo '{"result":{"root_pane":{"pane_id":"w1:p9"},"tab":{"tab_id":"w1:t9"}}}' ;;
  *) echo '{"result":{"type":"ok"}}' ;;
esac
"#;
            let path = dir.path().join("herdr");
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            let fake = Self { dir };
            fake.panes(&[]);
            fake.foreground("fish", true);
            fake
        }

        fn herdr(&self) -> Herdr {
            Herdr {
                bin: self.dir.path().join("herdr").display().to_string(),
                workspace: Some("w1".into()),
                origin: Some("w1:p1".into()),
                copier: |_| Ok(()),
            }
        }

        /// Panes as (pane id, agent session id).
        fn panes(&self, panes: &[(&str, &str)]) {
            let panes: Vec<Value> = panes
                .iter()
                .map(|(id, session)| {
                    serde_json::json!({
                        "pane_id": id,
                        "agent": "claude",
                        "agent_session": {"agent": "claude", "kind": "id", "value": session},
                    })
                })
                .collect();
            let json = serde_json::json!({"result": {"panes": panes}});
            std::fs::write(self.dir.path().join("panes.json"), json.to_string()).unwrap();
        }

        /// The origin pane's foreground process; `own` when it is the pane's shell itself.
        fn foreground(&self, name: &str, own: bool) {
            let pid = if own { 100 } else { 200 };
            let json = serde_json::json!({"result": {"process_info": {
                "shell_pid": 100,
                "foreground_process_group_id": pid,
                "foreground_processes": [{"name": name, "pid": pid}],
            }}});
            std::fs::write(self.dir.path().join("proc.json"), json.to_string()).unwrap();
        }

        fn fail(&self, call: &str) {
            std::fs::write(self.dir.path().join("fail"), call).unwrap();
        }

        /// Calls other than the read-only lookups.
        fn actions(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.path().join("calls"))
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.starts_with("pane|list|") && !l.starts_with("pane|process-info|"))
                .map(str::to_string)
                .collect()
        }
    }

    fn chosen(machine: &str) -> Chosen {
        Chosen {
            command: "cd /tmp && claude --resume s1".into(),
            machine: machine.into(),
            session_id: "s1".into(),
            cwd: Some("/tmp".into()),
            title: Some("修复  登录\n超时".into()),
        }
    }

    #[test]
    fn focuses_a_session_that_is_already_open() {
        let fake = Fake::new();
        fake.panes(&[("w1:p3", "other"), ("w2:p7", "s1")]);
        let outcome = fake.herdr().resume(&chosen(LOCAL)).unwrap();
        assert_eq!(outcome, Outcome::Focused("w2:p7".into()));
        let actions = fake.actions();
        assert_eq!(actions[0], "agent|focus|w2:p7|");
        assert!(actions[1].starts_with("notification|show|"));
    }

    #[test]
    fn remote_sessions_never_match_local_panes() {
        let fake = Fake::new();
        fake.panes(&[("w2:p7", "s1")]);
        let mut chosen = chosen("mm");
        chosen.command = "ssh -t mm 'cd /w && claude --resume s1'".into();
        let outcome = fake.herdr().resume(&chosen).unwrap();
        assert_eq!(outcome, Outcome::InPlace("w1:p1".into()));
        assert_eq!(
            fake.actions(),
            ["pane|run|w1:p1|ssh -t mm 'cd /w && claude --resume s1'|"]
        );
    }

    #[test]
    fn resumes_in_the_idle_shell_under_the_popup() {
        let fake = Fake::new();
        let outcome = fake.herdr().resume(&chosen(LOCAL)).unwrap();
        assert_eq!(outcome, Outcome::InPlace("w1:p1".into()));
        assert_eq!(
            fake.actions(),
            ["pane|run|w1:p1|cd /tmp && claude --resume s1|"]
        );
    }

    #[test]
    fn opens_a_tab_when_the_pane_is_busy() {
        let fake = Fake::new();
        fake.foreground("claude", false);
        let outcome = fake.herdr().resume(&chosen(LOCAL)).unwrap();
        assert_eq!(outcome, Outcome::NewTab("w1:p9".into()));
        assert_eq!(
            fake.actions(),
            [
                "tab|create|--label|修复 登录 超时|--focus|--workspace|w1|--cwd|/tmp|",
                "pane|run|w1:p9|cd /tmp && claude --resume s1|",
            ]
        );
    }

    #[test]
    fn a_command_running_as_the_pane_itself_is_not_a_prompt() {
        // Panes started with a command have it as their "shell".
        let fake = Fake::new();
        fake.foreground("claude", true);
        let outcome = fake.herdr().resume(&chosen("mm")).unwrap();
        assert_eq!(outcome, Outcome::NewTab("w1:p9".into()));
        // Remote: no local cwd, machine in the label.
        assert_eq!(
            fake.actions()[0],
            "tab|create|--label|mm·修复 登录 超时|--focus|--workspace|w1|"
        );
    }

    #[test]
    fn edit_fills_the_prompt_or_copies() {
        let fake = Fake::new();
        assert_eq!(
            fake.herdr().edit(&chosen(LOCAL)).unwrap(),
            Outcome::Filled("w1:p1".into())
        );
        assert_eq!(
            fake.actions(),
            ["pane|send-text|w1:p1|cd /tmp && claude --resume s1|"]
        );

        let fake = Fake::new();
        fake.foreground("codex", false);
        assert_eq!(fake.herdr().edit(&chosen(LOCAL)).unwrap(), Outcome::Copied);
        let actions = fake.actions();
        assert_eq!(actions.len(), 1);
        assert!(actions[0].starts_with("notification|show|已复制恢复命令|"));
    }

    #[test]
    fn herdr_errors_carry_its_message() {
        let fake = Fake::new();
        fake.foreground("claude", false);
        fake.fail("tab create");
        let err = fake.herdr().resume(&chosen(LOCAL)).unwrap_err();
        assert_eq!(format!("{err:#}"), "herdr tab create: it broke");
    }

    #[test]
    fn labels() {
        let mut c = chosen(LOCAL);
        c.title = None;
        c.cwd = Some("/Users/a/work/weather-cli/".into());
        assert_eq!(tab_label(&c), "weather-cli");
        c.cwd = None;
        assert_eq!(tab_label(&c), "s1");
        c.title = Some("一个非常非常非常非常长的会话标题需要截断".into());
        assert_eq!(text::display_width(&tab_label(&c)), LABEL_WIDTH - 1);
        assert!(tab_label(&c).ends_with('…'));
    }
}
