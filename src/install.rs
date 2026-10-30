//! `sessift remote install <machine>`: copy this binary to a remote machine over SSH.

use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::client::ssh_base;
use crate::config::{Config, Paths, TransportSpec};
use crate::output::Style;
use crate::timefmt::{human_bytes, human_duration};

pub struct InstallReport {
    pub uploaded: bool,
    pub remote_version: String,
}

pub async fn install(
    paths: &Paths,
    config: &Config,
    id: &str,
    force: bool,
) -> Result<InstallReport> {
    let style = Style::stderr();
    let spec = config.spec(id)?;
    let TransportSpec::Ssh {
        target,
        remote_command,
        options,
    } = &spec.transport
    else {
        bail!("machine '{id}' does not use SSH; install sessift there yourself");
    };
    if remote_command.contains('\'') || remote_command.chars().any(char::is_whitespace) {
        bail!("remote_command '{remote_command}' must be a plain path to install into");
    }
    let step = |msg: &str| eprintln!("{} {msg}", style.dim("→"));

    step(&format!("checking {id} ({})", spec.describe()));
    // Values come back on marked lines: login scripts may print anything else.
    let probe = format!(
        "echo SESSIFT_PLATFORM=$(uname -sm); if [ -f {remote_command} ]; then {}; fi",
        hash_command(remote_command)
    );
    let output = ssh_run(paths, target, options, &probe, None).await?;
    let remote_platform = marker(&output, "SESSIFT_PLATFORM").unwrap_or_default();
    let remote_hash = marker(&output, "SESSIFT_HASH")
        .and_then(|value| value.split_whitespace().next().map(str::to_string));
    let local_platform = local_platform();
    if remote_platform != local_platform {
        bail!(
            "{id} is {remote_platform}, this binary is built for {local_platform}; build sessift on {id} or cross-compile"
        );
    }

    let exe = std::env::current_exe().context("locate the sessift binary")?;
    let binary = std::fs::read(&exe).with_context(|| format!("read {}", exe.display()))?;
    let local_hash = hex(&Sha256::digest(&binary));
    let uploaded = if remote_hash.as_deref() == Some(local_hash.as_str()) && !force {
        step(&format!("{remote_command} is already this build"));
        false
    } else {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(&binary)?;
        let packed = gz.finish()?;
        step(&format!(
            "uploading {} ({} before compression)",
            human_bytes(packed.len() as u64),
            human_bytes(binary.len() as u64)
        ));
        let started = Instant::now();
        // Unpack next to the target, then rename, so a running server keeps its old inode.
        let script = format!(
            "set -e; umask 022; d=$(dirname {remote_command}); mkdir -p \"$d\"; t=\"$d/.sessift-upload.$$\"; \
             if ! gunzip -c > \"$t\"; then rm -f \"$t\"; exit 1; fi; chmod 755 \"$t\"; mv -f \"$t\" {remote_command}"
        );
        ssh_run(paths, target, options, &script, Some(packed)).await?;
        step(&format!(
            "uploaded in {}",
            human_duration(started.elapsed())
        ));
        true
    };

    let verify = format!(
        "echo SESSIFT_VERSION=$({remote_command} --version); {}",
        hash_command(remote_command)
    );
    let output = ssh_run(paths, target, options, &verify, None)
        .await
        .context("run the installed binary")?;
    let remote_version = marker(&output, "SESSIFT_VERSION").unwrap_or_default();
    let expected = format!("sessift {}", crate::VERSION);
    if remote_version != expected {
        bail!("{id} reports '{remote_version}', expected '{expected}'");
    }
    let installed_hash = marker(&output, "SESSIFT_HASH")
        .and_then(|value| value.split_whitespace().next().map(str::to_string));
    if installed_hash.as_deref() != Some(local_hash.as_str()) {
        bail!("{id}: installed file does not match this binary");
    }
    step(&format!("{id} runs {remote_version}"));
    Ok(InstallReport {
        uploaded,
        remote_version,
    })
}

/// Prints `SESSIFT_HASH=<sha256>  <path>` with whichever tool the remote has.
fn hash_command(path: &str) -> String {
    format!("echo SESSIFT_HASH=$(shasum -a 256 {path} 2>/dev/null || sha256sum {path})")
}

/// The value of the last `KEY=value` line in `output`.
fn marker(output: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    output
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix(&prefix))
        .map(|value| value.trim().to_string())
}

/// Run a POSIX shell snippet on `target` (whatever the login shell is) and return stdout.
async fn ssh_run(
    paths: &Paths,
    target: &str,
    options: &[String],
    script: &str,
    stdin: Option<Vec<u8>>,
) -> Result<String> {
    // The login shell (often fish) parses this line first; single quotes survive every shell.
    if script.contains('\'') {
        return Err(anyhow!(
            "internal: remote scripts must not contain single quotes"
        ));
    }
    let wrapped = format!("sh -c '{script}'");
    let mut command = ssh_base(paths, Duration::from_secs(20), true);
    command
        .args(options)
        .arg("--")
        .arg(target)
        .arg(wrapped)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().context("start ssh")?;
    if let Some(data) = stdin {
        let mut pipe = child.stdin.take().expect("piped stdin");
        let writer = tokio::spawn(async move {
            pipe.write_all(&data).await?;
            pipe.shutdown().await
        });
        let output = child.wait_with_output().await?;
        writer.await.ok();
        return finish(output);
    }
    finish(child.wait_with_output().await?)
}

fn finish(output: std::process::Output) -> Result<String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "remote command failed ({}): {}",
            output.status,
            stderr.trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `uname -sm` spelling of the platform this binary was built for.
pub fn local_platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "Darwin",
        "linux" => "Linux",
        other => other,
    };
    let arch = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "arm64",
        (_, arch) => arch,
    };
    format!("{os} {arch}")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_ignore_login_noise() {
        let output = "Welcome to mm!\nSESSIFT_PLATFORM=Darwin arm64\nlast login: today\nSESSIFT_HASH=abc123  /Users/x/.sessift/bin/sessift\n";
        assert_eq!(
            marker(output, "SESSIFT_PLATFORM").as_deref(),
            Some("Darwin arm64")
        );
        assert_eq!(
            marker(output, "SESSIFT_HASH").as_deref(),
            Some("abc123  /Users/x/.sessift/bin/sessift")
        );
        assert_eq!(marker(output, "SESSIFT_VERSION"), None);
    }
}
