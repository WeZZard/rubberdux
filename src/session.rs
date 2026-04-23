use std::fs;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

/// Unique identifier for a session, formatted as YYYY-MM-DD-hh-mm-ss-UTC.
#[derive(Debug, Clone)]
pub struct SessionId(pub chrono::DateTime<chrono::Utc>);

impl SessionId {
    pub fn now() -> Self {
        Self(chrono::Utc::now())
    }

    pub fn to_string(&self) -> String {
        self.0.format("%Y-%m-%d-%H-%M-%S-UTC").to_string()
    }

    pub fn from_string(s: &str) -> Option<Self> {
        let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d-%H-%M-%S-UTC").ok()?;
        Some(Self(chrono::DateTime::from_naive_utc_and_offset(naive, chrono::Utc)))
    }
}

/// Metadata for an agent (main or subagent).
#[derive(Debug, Serialize, Deserialize)]
pub struct AgentMetadata {
    pub start_time: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
}

impl AgentMetadata {
    pub fn for_main_agent(model: String) -> Self {
        Self {
            start_time: chrono::Utc::now().to_rfc3339(),
            model,
            subagent_name: None,
            parent_session_id: None,
            subagent_type: None,
        }
    }

    pub fn for_subagent(
        model: String,
        subagent_name: String,
        parent_session_id: String,
        subagent_type: String,
    ) -> Self {
        Self {
            start_time: chrono::Utc::now().to_rfc3339(),
            model,
            subagent_name: Some(subagent_name),
            parent_session_id: Some(parent_session_id),
            subagent_type: Some(subagent_type),
        }
    }
}

/// Manages session directories and symlinks.
pub struct SessionManager {
    pub home_dir: PathBuf,
    pub sessions_dir: PathBuf,
    pub latest_link: PathBuf,
}

impl SessionManager {
    /// Create a new SessionManager, resolving `$RUBBERDUX_HOME` or defaulting to `~/.rubberdux`.
    pub fn new() -> Self {
        let home_dir = Self::resolve_home();
        let sessions_dir = home_dir.join("sessions");
        let latest_link = home_dir.join("latest");
        Self {
            home_dir,
            sessions_dir,
            latest_link,
        }
    }

    fn resolve_home() -> PathBuf {
        std::env::var("RUBBERDUX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".rubberdux"))
    }

    /// Create a new session directory and return its ID and path.
    pub fn create_session(&self, model: String) -> Result<(SessionId, PathBuf), String> {
        let session_id = SessionId::now();
        let session_dir = self.sessions_dir.join(session_id.to_string());
        
        fs::create_dir_all(&session_dir)
            .map_err(|e| format!("Failed to create session dir: {}", e))?;

        let main_agent_dir = session_dir.join("agent_main");
        fs::create_dir_all(&main_agent_dir)
            .map_err(|e| format!("Failed to create agent_main dir: {}", e))?;

        let tool_results_dir = main_agent_dir.join("tool_results");
        fs::create_dir_all(&tool_results_dir)
            .map_err(|e| format!("Failed to create tool_results dir: {}", e))?;

        let metadata = AgentMetadata::for_main_agent(model);
        let metadata_path = main_agent_dir.join("metadata.json");
        Self::write_json(&metadata_path, &metadata)?;

        self.update_latest_symlink(&session_id)?;

        Ok((session_id, session_dir))
    }

    /// Create a subagent directory within a session.
    pub fn create_subagent_dir(
        &self,
        session_id: &SessionId,
        agent_id: &str,
        metadata: &AgentMetadata,
        prompt: &str,
    ) -> Result<PathBuf, String> {
        let session_dir = self.sessions_dir.join(session_id.to_string());
        let agent_dir = session_dir.join(format!("agent_{}", agent_id));
        
        fs::create_dir_all(&agent_dir)
            .map_err(|e| format!("Failed to create subagent dir: {}", e))?;

        let tool_results_dir = agent_dir.join("tool_results");
        fs::create_dir_all(&tool_results_dir)
            .map_err(|e| format!("Failed to create tool_results dir: {}", e))?;

        let metadata_path = agent_dir.join("metadata.json");
        Self::write_json(&metadata_path, metadata)?;

        let prompt_path = agent_dir.join("prompt.md");
        fs::write(&prompt_path, prompt)
            .map_err(|e| format!("Failed to write prompt.md: {}", e))?;

        Ok(agent_dir)
    }

    /// Get the path to a session directory.
    pub fn session_dir(&self, session_id: &SessionId) -> PathBuf {
        self.sessions_dir.join(session_id.to_string())
    }

    /// Get the path to an agent directory.
    pub fn agent_dir(&self, session_id: &SessionId, agent_id: &str) -> PathBuf {
        self.session_dir(session_id).join(format!("agent_{}", agent_id))
    }

    /// Get the path to the main agent directory.
    pub fn main_agent_dir(&self, session_id: &SessionId) -> PathBuf {
        self.session_dir(session_id).join("agent_main")
    }

    /// Get the path to the session log file.
    pub fn session_log_path(&self, session_id: &SessionId) -> PathBuf {
        self.session_dir(session_id).join("rubberdux.log")
    }

    /// Get the path to the latest symlink.
    pub fn latest_link(&self) -> &Path {
        &self.latest_link
    }

    /// Get the sessions directory.
    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    /// Update the `$RUBBERDUX_HOME/latest` symlink to point to the given session.
    pub fn update_latest_symlink(&self,
        session_id: &SessionId,
    ) -> Result<(), String> {
        let target = self.session_dir(session_id);
        
        // Remove existing symlink if present
        if self.latest_link.exists() {
            fs::remove_file(&self.latest_link)
                .map_err(|e| format!("Failed to remove existing latest symlink: {}", e))?;
        }

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &self.latest_link)
                .map_err(|e| format!("Failed to create latest symlink: {}", e))?;
        }

        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(&target, &self.latest_link)
                .map_err(|e| format!("Failed to create latest symlink: {}", e))?;
        }

        Ok(())
    }

    /// Create the `sessions` symlink in the project root pointing to `$RUBBERDUX_HOME/sessions`.
    pub fn create_project_symlink(project_root: &Path) -> Result<(), String> {
        let symlink_path = project_root.join("sessions");
        
        // Remove existing symlink or directory
        if symlink_path.exists() {
            if symlink_path.is_symlink() {
                fs::remove_file(&symlink_path)
                    .map_err(|e| format!("Failed to remove existing sessions symlink: {}", e))?;
            } else {
                return Err(format!(
                    "'sessions' already exists and is not a symlink: {}",
                    symlink_path.display()
                ));
            }
        }

        let target = Self::resolve_home().join("sessions");

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &symlink_path)
                .map_err(|e| format!("Failed to create project sessions symlink: {}", e))?;
        }

        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(&target, &symlink_path)
                .map_err(|e| format!("Failed to create project sessions symlink: {}", e))?;
        }

        Ok(())
    }

    fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
        let json = serde_json::to_string_pretty(value)
            .map_err(|e| format!("Failed to serialize JSON: {}", e))?;
        fs::write(path, json)
            .map_err(|e| format!("Failed to write JSON file: {}", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn temp_home() -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("rubberdux-test-{}", ts))
    }

    fn manager_with_home(home: &Path) -> SessionManager {
        let sessions_dir = home.join("sessions");
        let latest_link = home.join("latest");
        SessionManager {
            home_dir: home.to_path_buf(),
            sessions_dir,
            latest_link,
        }
    }

    #[test]
    fn test_session_id_format() {
        let id = SessionId::now();
        let s = id.to_string();
        // Should match YYYY-MM-DD-hh-mm-ss-UTC
        assert!(s.contains("-UTC"), "Session ID should end with -UTC: {}", s);
        assert_eq!(s.len(), 23, "Session ID should be 23 chars: {}", s);
    }

    #[test]
    fn test_session_id_roundtrip() {
        let id = SessionId::now();
        let s = id.to_string();
        let parsed = SessionId::from_string(&s);
        assert!(parsed.is_some());
        assert_eq!(s, parsed.unwrap().to_string());
    }

    #[test]
    fn test_session_dir_creation() {
        let home = temp_home();
        let mgr = manager_with_home(&home);
        let (session_id, session_dir) = mgr.create_session("kimi-for-coding".into()).unwrap();
        
        assert!(session_dir.exists());
        assert!(mgr.main_agent_dir(&session_id).exists());
        assert!(mgr.main_agent_dir(&session_id).join("tool_results").exists());
        assert!(mgr.main_agent_dir(&session_id).join("metadata.json").exists());
        
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn test_subagent_dir_creation() {
        let home = temp_home();
        let mgr = manager_with_home(&home);
        let (session_id, _) = mgr.create_session("kimi-for-coding".into()).unwrap();
        
        let metadata = AgentMetadata::for_subagent(
            "kimi-for-coding".into(),
            "web-search-agent".into(),
            session_id.to_string(),
            "GeneralPurpose".into(),
        );
        
        let agent_dir = mgr.create_subagent_dir(
            &session_id,
            "task-123",
            &metadata,
            "Search for latest news",
        ).unwrap();
        
        assert!(agent_dir.exists());
        assert!(agent_dir.join("metadata.json").exists());
        assert!(agent_dir.join("prompt.md").exists());
        assert!(agent_dir.join("tool_results").exists());
        
        let prompt_content = fs::read_to_string(agent_dir.join("prompt.md")).unwrap();
        assert_eq!(prompt_content, "Search for latest news");
        
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn test_metadata_serialization() {
        let metadata = AgentMetadata::for_main_agent("kimi-for-coding".into());
        let json = serde_json::to_string_pretty(&metadata).unwrap();
        assert!(json.contains("start_time"));
        assert!(json.contains("model"));
        assert!(!json.contains("subagent_name")); // Should be skipped
        
        let subagent = AgentMetadata::for_subagent(
            "kimi-for-coding".into(),
            "test-agent".into(),
            "session-123".into(),
            "Explore".into(),
        );
        let json = serde_json::to_string_pretty(&subagent).unwrap();
        assert!(json.contains("subagent_name"));
        assert!(json.contains("parent_session_id"));
        assert!(json.contains("subagent_type"));
    }

    #[test]
    fn test_latest_symlink_update() {
        let home = temp_home();
        let mgr = manager_with_home(&home);
        let (session_id, _) = mgr.create_session("kimi-for-coding".into()).unwrap();
        
        assert!(mgr.latest_link().exists());
        let target = fs::read_link(mgr.latest_link()).unwrap();
        assert_eq!(target, mgr.session_dir(&session_id));
        
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn test_rubberdux_home_default() {
        // Save and clear env
        let old = env::var("RUBBERDUX_HOME").ok();
        unsafe { env::remove_var("RUBBERDUX_HOME"); }
        
        let mgr = SessionManager::new();
        let expected = dirs::home_dir().unwrap().join(".rubberdux");
        assert_eq!(mgr.home_dir, expected);
        
        // Restore env
        if let Some(v) = old {
            unsafe { env::set_var("RUBBERDUX_HOME", v); }
        }
    }

    #[test]
    fn test_rubberdux_home_env_override() {
        let old = env::var("RUBBERDUX_HOME").ok();
        unsafe { env::set_var("RUBBERDUX_HOME", "/tmp/custom-rubberdux"); }
        
        let mgr = SessionManager::new();
        assert_eq!(mgr.home_dir, PathBuf::from("/tmp/custom-rubberdux"));
        
        // Restore env
        if let Some(v) = old {
            unsafe { env::set_var("RUBBERDUX_HOME", v); }
        } else {
            unsafe { env::remove_var("RUBBERDUX_HOME"); }
        }
    }

    #[test]
    fn test_session_log_path() {
        let home = temp_home();
        let mgr = manager_with_home(&home);
        let (session_id, _) = mgr.create_session("kimi-for-coding".into()).unwrap();
        
        let log_path = mgr.session_log_path(&session_id);
        assert!(log_path.to_string_lossy().contains("rubberdux.log"));
        
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn test_agent_metadata_main() {
        let meta = AgentMetadata::for_main_agent("gpt-4".into());
        assert_eq!(meta.model, "gpt-4");
        assert!(meta.subagent_name.is_none());
        assert!(meta.parent_session_id.is_none());
        assert!(meta.subagent_type.is_none());
    }

    #[test]
    fn test_agent_metadata_subagent() {
        let meta = AgentMetadata::for_subagent(
            "gpt-4".into(),
            "search-agent".into(),
            "sess-1".into(),
            "Explore".into(),
        );
        assert_eq!(meta.model, "gpt-4");
        assert_eq!(meta.subagent_name, Some("search-agent".to_string()));
        assert_eq!(meta.parent_session_id, Some("sess-1".to_string()));
        assert_eq!(meta.subagent_type, Some("Explore".to_string()));
    }

    #[test]
    fn test_main_agent_dir() {
        let home = temp_home();
        let mgr = manager_with_home(&home);
        let (session_id, _) = mgr.create_session("kimi".into()).unwrap();
        
        let main_dir = mgr.main_agent_dir(&session_id);
        assert!(main_dir.to_string_lossy().contains("agent_main"));
        assert!(main_dir.exists());
        
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn test_create_session_idempotent() {
        let home = temp_home();
        let mgr = manager_with_home(&home);
        
        // Create session
        let (id1, dir1) = mgr.create_session("kimi".into()).unwrap();
        
        // Verify latest symlink
        let latest = fs::read_link(mgr.latest_link()).unwrap();
        assert_eq!(latest, dir1);
        
        // Create another session
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let (id2, dir2) = mgr.create_session("kimi".into()).unwrap();
        
        // Verify latest updated
        let latest = fs::read_link(mgr.latest_link()).unwrap();
        assert_eq!(latest, dir2);
        assert_ne!(id1.to_string(), id2.to_string());
        
        let _ = fs::remove_dir_all(&home);
    }
}