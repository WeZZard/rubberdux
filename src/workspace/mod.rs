pub mod entity;

use std::path::PathBuf;

use crate::error::Error;

pub struct Workspace {
    pub root: PathBuf,
}

impl Workspace {
    pub fn new() -> Self {
        Self {
            root: Self::resolve_root(),
        }
    }

    fn resolve_root() -> PathBuf {
        std::env::var("RUBBERDUX_WORKSPACE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".rubberdux")
                    .join("workspace")
            })
    }

    pub fn ensure_dirs(&self) -> Result<(), Error> {
        for dir in [
            self.root.clone(),
            self.root.join("projects"),
            self.root.join("tasks"),
            self.root.join("archives"),
            self.root.join("archives").join("projects"),
            self.root.join("archives").join("tasks"),
        ] {
            std::fs::create_dir_all(&dir).map_err(|e| {
                Error::Workspace(format!("Failed to create {}: {}", dir.display(), e))
            })?;
        }
        Ok(())
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }

    pub fn tasks_dir(&self) -> PathBuf {
        self.root.join("tasks")
    }

    pub fn archives_dir(&self) -> PathBuf {
        self.root.join("archives")
    }
}

pub use crate::frontmatter::{parse_yaml_front_matter, format_yaml_front_matter};

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn temp_workspace() -> (PathBuf, Workspace) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("rubberdux-workspace-test-{}", ts));
        let ws = Workspace { root: root.clone() };
        (root, ws)
    }

    #[test]
    fn test_ensure_dirs_creates_all_directories() {
        let (root, ws) = temp_workspace();
        ws.ensure_dirs().unwrap();

        assert!(root.join("projects").exists());
        assert!(root.join("tasks").exists());
        assert!(root.join("archives").exists());
        assert!(root.join("archives").join("projects").exists());
        assert!(root.join("archives").join("tasks").exists());
        assert!(!root.join("artifacts").exists());
        assert!(!root.join("resources").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_ensure_dirs_idempotent() {
        let (root, ws) = temp_workspace();
        ws.ensure_dirs().unwrap();
        ws.ensure_dirs().unwrap();

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_path_accessors() {
        let (root, ws) = temp_workspace();

        assert_eq!(ws.projects_dir(), root.join("projects"));
        assert_eq!(ws.tasks_dir(), root.join("tasks"));
        assert_eq!(ws.archives_dir(), root.join("archives"));
    }

    #[test]
    fn test_workspace_root_env_override() {
        let old = env::var("RUBBERDUX_WORKSPACE_DIR").ok();
        unsafe {
            env::set_var("RUBBERDUX_WORKSPACE_DIR", "/tmp/custom-workspace");
        }

        let ws = Workspace::new();
        assert_eq!(ws.root, PathBuf::from("/tmp/custom-workspace"));

        if let Some(v) = old {
            unsafe {
                env::set_var("RUBBERDUX_WORKSPACE_DIR", v);
            }
        } else {
            unsafe {
                env::remove_var("RUBBERDUX_WORKSPACE_DIR");
            }
        }
    }

    #[test]
    fn test_workspace_root_default() {
        let old = env::var("RUBBERDUX_WORKSPACE_DIR").ok();
        unsafe {
            env::remove_var("RUBBERDUX_WORKSPACE_DIR");
        }

        let ws = Workspace::new();
        let expected = dirs::home_dir().unwrap().join(".rubberdux").join("workspace");
        assert_eq!(ws.root, expected);

        if let Some(v) = old {
            unsafe {
                env::set_var("RUBBERDUX_WORKSPACE_DIR", v);
            }
        }
    }

}
