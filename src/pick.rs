//! Shared by the session picker: resume commands, shell quoting, the clipboard and the shell
//! key bindings that open the picker.
//!
//! The picker talks to the shell binding through stdout: nothing (cancelled), or two lines,
//! an action (`run` executes, `edit` only fills the prompt) and the command. Inside Herdr the
//! picker hands the choice to `crate::herdr` instead.

use std::io::Write;
use std::process::Stdio;

use anyhow::{Result, anyhow, bail};

use crate::config::{LOCAL, TransportSpec};
use crate::fanout::{Runner, merge_sessions};
use crate::model::{Filters, SessionSummary, SessionsRequest};
use crate::proto::Op;
use crate::text;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ShellKind {
    Fish,
    Zsh,
    Bash,
    /// POSIX sh quoting.
    Sh,
}

impl ShellKind {
    /// The shell a path like `$SHELL` or `-fish` names; anything unknown quotes as POSIX sh.
    pub fn from_path(path: &str) -> Self {
        let name = path.rsplit('/').next().unwrap_or(path);
        match name.trim_start_matches('-') {
            "fish" => Self::Fish,
            "zsh" => Self::Zsh,
            "bash" => Self::Bash,
            _ => Self::Sh,
        }
    }

    /// The user's login shell, from `$SHELL`.
    pub fn from_env() -> Self {
        Self::from_path(&std::env::var("SHELL").unwrap_or_default())
    }
}

/// Quote one word for `shell`.
pub fn quote(word: &str, shell: ShellKind) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:=@%+,".contains(&b))
    {
        return word.to_string();
    }
    match shell {
        // Inside fish single quotes only \\ and \' are escapes.
        ShellKind::Fish => format!("'{}'", word.replace('\\', "\\\\").replace('\'', "\\'")),
        ShellKind::Zsh | ShellKind::Bash | ShellKind::Sh => {
            format!("'{}'", word.replace('\'', "'\\''"))
        }
    }
}

/// A session chosen in the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub machine: String,
    pub session_id: String,
    pub source_path: String,
}

/// What the picker hands back: the resume command and where the session lives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Chosen {
    pub command: String,
    pub machine: String,
    pub session_id: String,
    /// The session's directory on its own machine.
    pub cwd: Option<String>,
    pub title: Option<String>,
}

/// Look the session up on its machine and build the command that resumes it from here.
pub async fn resume_command(runner: &Runner, target: &Target, shell: ShellKind) -> Chosen {
    let request = SessionsRequest {
        limit: 20,
        filters: Filters {
            session_id: Some(target.session_id.clone()),
            ..Filters::default()
        },
        ..SessionsRequest::default()
    };
    let timeout = runner.config().timeout();
    let outcomes = runner
        .run(std::slice::from_ref(&target.machine), timeout, |_| {
            Op::Sessions(request.clone())
        })
        .await;
    let sessions = merge_sessions(&outcomes, 0, 20);
    let summary = sessions
        .iter()
        .find(|s| s.item.source_path == target.source_path)
        .or_else(|| sessions.first())
        .map(|s| &s.item);
    let transport = (target.machine != LOCAL)
        .then(|| runner.config().spec(&target.machine).ok())
        .flatten()
        .map(|spec| spec.transport);
    Chosen {
        command: build_resume(target, summary, transport.as_ref(), shell),
        machine: target.machine.clone(),
        session_id: target.session_id.clone(),
        cwd: summary.and_then(|s| s.cwd.clone()),
        title: summary.and_then(|s| s.title.clone()),
    }
}

/// Pure part of [`resume_command`].
pub fn build_resume(
    target: &Target,
    summary: Option<&SessionSummary>,
    transport: Option<&TransportSpec>,
    shell: ShellKind,
) -> String {
    let read = || {
        let mut parts = vec![
            "sessift".to_string(),
            "read".into(),
            quote(&target.session_id, shell),
            "-m".into(),
            quote(&target.machine, shell),
        ];
        if !target.source_path.is_empty() {
            parts.push("--path".into());
            parts.push(quote(&target.source_path, shell));
        }
        parts.join(" ")
    };
    let Some(resume) = summary.and_then(|s| s.resume_cmd.as_deref()) else {
        return read();
    };
    let resume = text::sanitize_line(resume);
    if target.machine == LOCAL {
        return resume;
    }
    match transport {
        // The remote login shell runs the command as given, so it stays one word here.
        Some(TransportSpec::Ssh { target: host, .. }) => {
            format!("ssh -t {} {}", quote(host, shell), quote(&resume, shell))
        }
        _ => read(),
    }
}

/// Copy to the clipboard: a native tool when there is one, else an OSC 52 escape.
pub fn copy(value: &str) -> Result<()> {
    let tools: [&[&str]; 3] = [
        &["pbcopy"],
        &["wl-copy"],
        &["xclip", "-selection", "clipboard"],
    ];
    for tool in tools {
        let child = std::process::Command::new(tool[0])
            .args(&tool[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(value.as_bytes())?;
            }
            if child.wait()?.success() {
                return Ok(());
            }
        }
    }
    let mut tty = std::fs::OpenOptions::new().write(true).open("/dev/tty")?;
    write!(
        tty,
        "\x1b]52;c;{}\x07",
        base64_lite::encode(value.as_bytes())
    )?;
    Ok(())
}

mod base64_lite {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }
}

/// Key names in fish 4 notation: `alt-m`, `ctrl-o`.
pub fn shell_snippet(shell: ShellKind, key: &str) -> Result<String> {
    let (modifier, letter) = key
        .split_once('-')
        .filter(|(_, l)| l.len() == 1 && l.chars().all(|c| c.is_ascii_lowercase()))
        .ok_or_else(|| anyhow!("key must look like alt-m or ctrl-o, got '{key}'"))?;
    let snippet = match shell {
        ShellKind::Fish => {
            let legacy = match modifier {
                "alt" => format!("\\e{letter}"),
                "ctrl" => format!("\\c{letter}"),
                _ => bail!("unsupported modifier '{modifier}'"),
            };
            format!(
                r#"# sessift: {key} opens the session picker; Enter resumes the session, e puts the command on the prompt.
function sessift-widget -d "Pick an agent session with sessift"
    set -l out (command sessift pick --shell fish)
    if test (count $out) -ge 2
        commandline --replace -- $out[2]
        if test "$out[1]" = run
            commandline --function execute
            return
        end
    end
    commandline --function repaint
end
if test (string split -f1 . $version) -ge 4
    bind {key} sessift-widget
    bind -M insert {key} sessift-widget 2>/dev/null
else
    bind {legacy} sessift-widget
    bind -M insert {legacy} sessift-widget 2>/dev/null
end
"#
            )
        }
        ShellKind::Zsh => {
            let seq = match modifier {
                "alt" => format!("\\e{letter}"),
                "ctrl" => format!("^{}", letter.to_ascii_uppercase()),
                _ => bail!("unsupported modifier '{modifier}'"),
            };
            format!(
                r#"# sessift: {key} opens the session picker; Enter resumes the session, e puts the command on the prompt.
sessift-widget() {{
  local out action cmd
  out="$(command sessift pick --shell zsh)"
  action="${{out%%$'\n'*}}"
  cmd="${{out#*$'\n'}}"
  if [[ -n $out && $action != "$out" ]]; then
    BUFFER="$cmd"
    CURSOR=${{#BUFFER}}
    if [[ $action == run ]]; then
      zle accept-line
      return
    fi
  fi
  zle reset-prompt
}}
zle -N sessift-widget
bindkey -M emacs '{seq}' sessift-widget
bindkey -M viins '{seq}' sessift-widget
"#
            )
        }
        ShellKind::Bash => {
            let seq = match modifier {
                "alt" => format!("\\e{letter}"),
                "ctrl" => format!("\\C-{letter}"),
                _ => bail!("unsupported modifier '{modifier}'"),
            };
            format!(
                r#"# sessift: {key} opens the session picker; the chosen command lands on the prompt.
__sessift_widget() {{
  local out action cmd
  out="$(command sessift pick --shell bash)"
  action="${{out%%$'\n'*}}"
  cmd="${{out#*$'\n'}}"
  if [[ -n $out && $action != "$out" ]]; then
    READLINE_LINE="$cmd"
    READLINE_POINT=${{#cmd}}
  fi
}}
bind -x '"{seq}": __sessift_widget'
"#
            )
        }
        ShellKind::Sh => bail!("shell integration is available for fish, zsh and bash"),
    };
    Ok(snippet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_from_path() {
        assert_eq!(
            ShellKind::from_path("/opt/homebrew/bin/fish"),
            ShellKind::Fish
        );
        assert_eq!(ShellKind::from_path("-zsh"), ShellKind::Zsh);
        assert_eq!(ShellKind::from_path("bash"), ShellKind::Bash);
        assert_eq!(ShellKind::from_path("/bin/dash"), ShellKind::Sh);
        assert_eq!(ShellKind::from_path(""), ShellKind::Sh);
    }

    #[test]
    fn quoting_per_shell() {
        assert_eq!(quote("plain-id_1", ShellKind::Fish), "plain-id_1");
        assert_eq!(
            quote("cd '/a b' && x", ShellKind::Fish),
            r"'cd \'/a b\' && x'"
        );
        assert_eq!(
            quote("cd '/a b' && x", ShellKind::Zsh),
            r"'cd '\''/a b'\'' && x'"
        );
        assert_eq!(quote(r"a\b", ShellKind::Fish), r"'a\\b'");
        assert_eq!(quote(r"a\b", ShellKind::Bash), r"'a\b'");
        assert_eq!(quote("", ShellKind::Sh), "''");
    }

    fn target(machine: &str) -> Target {
        Target {
            machine: machine.into(),
            session_id: "abc".into(),
            source_path: "/p/abc.jsonl".into(),
        }
    }

    fn summary(resume: Option<&str>) -> SessionSummary {
        SessionSummary {
            source: "claude".into(),
            session_id: "abc".into(),
            source_path: "/p/abc.jsonl".into(),
            project: "p".into(),
            cwd: None,
            title: None,
            started_ms: None,
            last_ms: 0,
            message_count: None,
            kind: None,
            resume_cmd: resume.map(str::to_string),
        }
    }

    #[test]
    fn resume_commands() {
        let resume = "cd '/w/my proj' && claude --resume abc";
        let ssh = TransportSpec::Ssh {
            target: "mm".into(),
            remote_command: "x".into(),
            options: vec![],
        };
        assert_eq!(
            build_resume(
                &target("local"),
                Some(&summary(Some(resume))),
                None,
                ShellKind::Fish
            ),
            resume
        );
        assert_eq!(
            build_resume(
                &target("mm"),
                Some(&summary(Some(resume))),
                Some(&ssh),
                ShellKind::Fish
            ),
            r"ssh -t mm 'cd \'/w/my proj\' && claude --resume abc'"
        );
        assert_eq!(
            build_resume(
                &target("mm"),
                Some(&summary(Some(resume))),
                Some(&ssh),
                ShellKind::Zsh
            ),
            r"ssh -t mm 'cd '\''/w/my proj'\'' && claude --resume abc'"
        );
        assert_eq!(
            build_resume(
                &target("mm"),
                Some(&summary(None)),
                Some(&ssh),
                ShellKind::Fish
            ),
            "sessift read abc -m mm --path /p/abc.jsonl"
        );
        assert_eq!(
            build_resume(&target("local"), None, None, ShellKind::Bash),
            "sessift read abc -m local --path /p/abc.jsonl"
        );
    }

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64_lite::encode(b""), "");
        assert_eq!(base64_lite::encode(b"f"), "Zg==");
        assert_eq!(base64_lite::encode(b"fo"), "Zm8=");
        assert_eq!(base64_lite::encode(b"foo"), "Zm9v");
        assert_eq!(base64_lite::encode("会话".as_bytes()), "5Lya6K+d");
    }

    #[test]
    fn snippets_bind_the_key() {
        let fish = shell_snippet(ShellKind::Fish, "alt-m").unwrap();
        assert!(fish.contains("bind alt-m sessift-widget"));
        assert!(fish.contains(r"bind \em sessift-widget"));
        assert!(fish.contains("commandline --function execute"));
        assert!(
            shell_snippet(ShellKind::Zsh, "ctrl-o")
                .unwrap()
                .contains("bindkey -M emacs '^O'")
        );
        assert!(
            shell_snippet(ShellKind::Bash, "alt-m")
                .unwrap()
                .contains(r#""\em": __sessift_widget"#)
        );
        assert!(shell_snippet(ShellKind::Fish, "m").is_err());
        assert!(shell_snippet(ShellKind::Fish, "alt-mm").is_err());
    }
}
