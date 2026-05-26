pub mod entity;

use std::path::{Path, PathBuf};

use crate::error::Error;

pub struct Mindset {
    pub root: PathBuf,
}

impl Mindset {
    pub fn new() -> Self {
        Self {
            root: Self::resolve_root(),
        }
    }

    fn resolve_root() -> PathBuf {
        std::env::var("RUBBERDUX_MINDSET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".rubberdux")
                    .join("mindset")
            })
    }

    pub fn ensure_dirs(&self) -> Result<(), Error> {
        for dir in [self.root.clone(), self.root.join("responsibilities")] {
            std::fs::create_dir_all(&dir).map_err(|e| {
                Error::Mindset(format!("Failed to create {}: {}", dir.display(), e))
            })?;
        }
        Ok(())
    }

    pub fn identity_path(&self) -> PathBuf {
        self.root.join("IDENTITY.md")
    }

    pub fn soul_path(&self) -> PathBuf {
        self.root.join("SOUL.md")
    }

    pub fn responsibilities_dir(&self) -> PathBuf {
        self.root.join("responsibilities")
    }

    pub fn seed_defaults_if_empty(&self, seed_dir: &Path) {
        for name in ["IDENTITY.md", "SOUL.md"] {
            let dest = self.root.join(name);
            if !dest.exists() {
                let src = seed_dir.join(name);
                if src.exists() && let Ok(content) = std::fs::read_to_string(&src) {
                    let _ = std::fs::write(&dest, content);
                    log::info!("Seeded mindset file: {:?} -> {:?}", src, dest);
                }
            }
        }
    }
}

pub fn convention() -> crate::guardrail::Convention {
    crate::guardrail::Convention {
        name: "Responsibilities".into(),
        guidance: include_str!("convention.md").into(),
        pre_guardrails: vec![],
        post_guardrails: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::env;

    fn temp_mindset() -> (PathBuf, Mindset) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("rubberdux-mindset-test-{}", ts));
        let ms = Mindset { root: root.clone() };
        (root, ms)
    }

    #[test]
    fn test_ensure_dirs_creates_root() {
        let (root, ms) = temp_mindset();
        ms.ensure_dirs().unwrap();
        assert!(root.exists());
        assert!(root.join("responsibilities").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_ensure_dirs_idempotent() {
        let (root, ms) = temp_mindset();
        ms.ensure_dirs().unwrap();
        ms.ensure_dirs().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_path_accessors() {
        let (root, ms) = temp_mindset();
        assert_eq!(ms.identity_path(), root.join("IDENTITY.md"));
        assert_eq!(ms.soul_path(), root.join("SOUL.md"));
        assert_eq!(ms.responsibilities_dir(), root.join("responsibilities"));
    }

    #[test]
    #[serial]
    fn test_mindset_root_env_override() {
        let old = env::var("RUBBERDUX_MINDSET_DIR").ok();
        unsafe {
            env::set_var("RUBBERDUX_MINDSET_DIR", "/tmp/custom-mindset");
        }

        let ms = Mindset::new();
        assert_eq!(ms.root, PathBuf::from("/tmp/custom-mindset"));

        if let Some(v) = old {
            unsafe { env::set_var("RUBBERDUX_MINDSET_DIR", v); }
        } else {
            unsafe { env::remove_var("RUBBERDUX_MINDSET_DIR"); }
        }
    }

    #[test]
    #[serial]
    fn test_mindset_root_default() {
        let old = env::var("RUBBERDUX_MINDSET_DIR").ok();
        unsafe {
            env::remove_var("RUBBERDUX_MINDSET_DIR");
        }

        let ms = Mindset::new();
        let expected = dirs::home_dir().unwrap().join(".rubberdux").join("mindset");
        assert_eq!(ms.root, expected);

        if let Some(v) = old {
            unsafe { env::set_var("RUBBERDUX_MINDSET_DIR", v); }
        }
    }

    #[test]
    fn test_seed_defaults_copies_when_empty() {
        let (root, ms) = temp_mindset();
        ms.ensure_dirs().unwrap();

        let seed_dir = env::temp_dir().join(format!("rubberdux-seed-test-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&seed_dir).unwrap();
        std::fs::write(seed_dir.join("IDENTITY.md"), "Test identity").unwrap();
        std::fs::write(seed_dir.join("SOUL.md"), "Test soul").unwrap();

        ms.seed_defaults_if_empty(&seed_dir);

        assert_eq!(std::fs::read_to_string(ms.identity_path()).unwrap(), "Test identity");
        assert_eq!(std::fs::read_to_string(ms.soul_path()).unwrap(), "Test soul");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&seed_dir);
    }

    #[test]
    fn test_seed_defaults_skips_when_present() {
        let (root, ms) = temp_mindset();
        ms.ensure_dirs().unwrap();

        std::fs::write(ms.identity_path(), "Existing identity").unwrap();
        std::fs::write(ms.soul_path(), "Existing soul").unwrap();

        let seed_dir = env::temp_dir().join(format!("rubberdux-seed-skip-test-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&seed_dir).unwrap();
        std::fs::write(seed_dir.join("IDENTITY.md"), "New identity").unwrap();
        std::fs::write(seed_dir.join("SOUL.md"), "New soul").unwrap();

        ms.seed_defaults_if_empty(&seed_dir);

        assert_eq!(std::fs::read_to_string(ms.identity_path()).unwrap(), "Existing identity");
        assert_eq!(std::fs::read_to_string(ms.soul_path()).unwrap(), "Existing soul");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&seed_dir);
    }
}
