//! The agent skill (`skills/sessift/SKILL.md`), built into the binary so the installed copy
//! always describes the binary that installed it.
//!
//! Codex, Pi, OpenCode and Grok read skills from `~/.agents/skills`; Claude Code only reads
//! `~/.claude/skills`, which gets a link to the copy there.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

pub const SKILL_MD: &str = include_str!("../skills/sessift/SKILL.md");
const NAME: &str = "sessift";

/// Install into each of `dirs`, or the default places under `home` when it is empty. Returns
/// one line per place for the user.
pub fn install(home: &Path, dirs: &[PathBuf]) -> Result<Vec<String>> {
    if !dirs.is_empty() {
        return dirs.iter().map(|dir| write(&dir.join(NAME))).collect();
    }
    let shared = home.join(".agents/skills").join(NAME);
    let mut report = vec![write(&shared)?];
    if home.join(".claude").is_dir() {
        report.push(link(&home.join(".claude/skills").join(NAME), &shared)?);
    }
    Ok(report)
}

/// Whether the copy in `~/.agents/skills` is missing or differs from this binary's.
pub fn stale(home: &Path) -> Option<PathBuf> {
    let path = home.join(".agents/skills").join(NAME).join("SKILL.md");
    match std::fs::read_to_string(&path) {
        Ok(text) if text == SKILL_MD => None,
        Ok(_) => Some(path),
        Err(_) => None,
    }
}

fn write(dir: &Path) -> Result<String> {
    let path = dir.join("SKILL.md");
    if std::fs::read_to_string(&path).is_ok_and(|text| text == SKILL_MD) {
        return Ok(format!("{} is up to date", path.display()));
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(&path, SKILL_MD).with_context(|| format!("write {}", path.display()))?;
    Ok(format!("wrote {}", path.display()))
}

/// Make `at` a link to `target`; a directory already there gets its own copy.
fn link(at: &Path, target: &Path) -> Result<String> {
    match std::fs::symlink_metadata(at) {
        Err(_) => {
            let parent = at
                .parent()
                .ok_or_else(|| anyhow!("no parent: {}", at.display()))?;
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
            std::os::unix::fs::symlink(target, at)
                .with_context(|| format!("link {} to {}", at.display(), target.display()))?;
            Ok(format!("linked {} -> {}", at.display(), target.display()))
        }
        Ok(meta) if meta.file_type().is_symlink() => {
            let points_to = std::fs::read_link(at)?;
            let resolved = at.parent().unwrap_or(at).join(&points_to);
            if same_file(&resolved, target) {
                Ok(format!("{} links to it", at.display()))
            } else {
                Ok(format!(
                    "left {} alone: it links to {}",
                    at.display(),
                    points_to.display()
                ))
            }
        }
        Ok(meta) if meta.is_dir() => write(at),
        Ok(_) => Ok(format!("left {} alone: not a directory", at.display())),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skill_has_its_front_matter() {
        assert!(SKILL_MD.starts_with("---\nname: sessift\ndescription: "));
    }

    #[test]
    fn installs_once_and_links_claude_to_it() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        std::fs::create_dir(home.join(".claude")).unwrap();

        let report = install(home, &[]).unwrap();
        assert!(report[0].starts_with("wrote "), "{report:?}");
        assert!(report[1].starts_with("linked "), "{report:?}");
        let via_claude = home.join(".claude/skills/sessift/SKILL.md");
        assert_eq!(std::fs::read_to_string(via_claude).unwrap(), SKILL_MD);
        assert_eq!(stale(home), None);

        let again = install(home, &[]).unwrap();
        assert!(again[0].ends_with("is up to date"), "{again:?}");
        assert!(again[1].ends_with("links to it"), "{again:?}");

        // An older copy is reported and replaced.
        let shared = home.join(".agents/skills/sessift/SKILL.md");
        std::fs::write(&shared, "old").unwrap();
        assert_eq!(stale(home), Some(shared.clone()));
        install(home, &[]).unwrap();
        assert_eq!(std::fs::read_to_string(&shared).unwrap(), SKILL_MD);
    }

    #[test]
    fn without_claude_or_into_given_dirs() {
        let home = tempfile::tempdir().unwrap();
        let report = install(home.path(), &[]).unwrap();
        assert_eq!(report.len(), 1, "no ~/.claude, no link");

        let dir = home.path().join("custom");
        let report = install(home.path(), std::slice::from_ref(&dir)).unwrap();
        assert_eq!(
            report,
            [format!("wrote {}", dir.join("sessift/SKILL.md").display())]
        );
    }

    #[test]
    fn a_link_elsewhere_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let other = home.join("elsewhere");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(home.join(".claude/skills")).unwrap();
        std::os::unix::fs::symlink(&other, home.join(".claude/skills/sessift")).unwrap();
        let report = install(home, &[]).unwrap();
        assert!(report[1].starts_with("left "), "{report:?}");
        assert!(!other.join("SKILL.md").exists());
    }
}
