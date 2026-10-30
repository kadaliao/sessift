//! Project names from working directories: the git repository's directory name (the main
//! repository for worktrees), else the last path component.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct ProjectNames {
    cache: HashMap<String, String>,
}

impl ProjectNames {
    pub fn name(&mut self, cwd: &str) -> String {
        if let Some(name) = self.cache.get(cwd) {
            return name.clone();
        }
        let name = project_name(Path::new(cwd));
        self.cache.insert(cwd.to_string(), name.clone());
        name
    }
}

pub fn project_name(cwd: &Path) -> String {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        // Stop at the home directory: a dotfiles repo there is not "the project".
        if std::env::var_os("HOME").is_some_and(|home| Path::new(&home) == d) {
            break;
        }
        let git = d.join(".git");
        if let Ok(meta) = std::fs::symlink_metadata(&git) {
            if meta.is_file()
                && let Some(main) = worktree_main(&git)
            {
                return file_name(&main);
            }
            return file_name(d);
        }
        dir = d.parent();
    }
    file_name(cwd)
}

/// For a worktree's `.git` file (`gitdir: /repo/.git/worktrees/name`), the main repository.
fn worktree_main(git_file: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(git_file).ok()?;
    let gitdir = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    let gitdir = Path::new(gitdir);
    let parts: Vec<_> = gitdir.components().collect();
    let at = parts
        .windows(2)
        .rposition(|w| w[0].as_os_str() == ".git" && w[1].as_os_str() == "worktrees")?;
    Some(parts[..at].iter().collect())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_root_worktree_and_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("shop-api");
        std::fs::create_dir_all(repo.join(".git/worktrees/feature")).unwrap();
        std::fs::create_dir_all(repo.join("src/handlers")).unwrap();
        assert_eq!(project_name(&repo.join("src/handlers")), "shop-api");
        let wt = dir.path().join("elsewhere/feature");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!(
                "gitdir: {}\n",
                repo.join(".git/worktrees/feature").display()
            ),
        )
        .unwrap();
        assert_eq!(project_name(&wt), "shop-api");
        assert_eq!(project_name(Path::new("/nonexistent/place/blog")), "blog");
    }
}
