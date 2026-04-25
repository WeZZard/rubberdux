use std::path::{Path, PathBuf};

const DEFAULT_PROMPT_DIR: &str = "./prompts";

pub fn prompt_dir() -> PathBuf {
    std::env::var("RUBBERDUX_PROMPT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_PROMPT_DIR))
}

/// Loads prompt parts from the prompt directory in order: IDENTITY.md, SOUL.md.
pub fn load_prompt_parts(prompt_dir: &Path) -> Vec<String> {
    let mut parts = Vec::new();
    for name in ["IDENTITY.md", "SOUL.md"] {
        let path = prompt_dir.join(name);
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                log::info!("Loaded prompt part: {:?}", path);
                parts.push(content);
            }
            Err(e) => {
                log::warn!("Failed to load prompt part {:?}: {}", path, e);
            }
        }
    }
    parts
}

/// Subagent preamble selected by type. Prepended to the system prompt
/// for subagent loops to scope their capabilities.
pub fn subagent_preamble(subagent_type: crate::tool::SubagentType) -> &'static str {
    use crate::tool::SubagentType;
    match subagent_type {
        SubagentType::Explore => include_str!("agents/EXPLORE_PREAMBLE.md"),
        SubagentType::Plan => include_str!("agents/PLAN_PREAMBLE.md"),
        SubagentType::GeneralPurpose => include_str!("agents/GP_PREAMBLE.md"),
        SubagentType::ComputerUse => include_str!("agents/COMPUTER_USE_PREAMBLE.md"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::SubagentType;

    #[test]
    fn test_preambles_non_empty() {
        for ty in [
            SubagentType::Explore,
            SubagentType::Plan,
            SubagentType::GeneralPurpose,
            SubagentType::ComputerUse,
        ] {
            let preamble = subagent_preamble(ty);
            assert!(
                !preamble.trim().is_empty(),
                "{:?} preamble should be non-empty",
                ty
            );
        }
    }

    #[test]
    fn test_preambles_distinct() {
        let explore = subagent_preamble(SubagentType::Explore);
        let plan = subagent_preamble(SubagentType::Plan);
        let gp = subagent_preamble(SubagentType::GeneralPurpose);
        let cu = subagent_preamble(SubagentType::ComputerUse);
        assert_ne!(explore, plan, "Explore and Plan preambles must differ");
        assert_ne!(
            explore, gp,
            "Explore and GeneralPurpose preambles must differ"
        );
        assert_ne!(plan, gp, "Plan and GeneralPurpose preambles must differ");
        assert_ne!(cu, explore, "ComputerUse and Explore preambles must differ");
        assert_ne!(cu, plan, "ComputerUse and Plan preambles must differ");
        assert_ne!(
            cu, gp,
            "ComputerUse and GeneralPurpose preambles must differ"
        );
    }
}

/// OODA reasoning principles. Cannot be modified by end users.
const OODA: &str = include_str!("common/OODA.md");

/// Tool usage principles. Cannot be modified by end users.
const TOOL_USE: &str = include_str!("common/TOOL_USE.md");

/// Built-in guardrails that cannot be modified by end users.
const GUARDRAILS: &str = include_str!("common/GUARDRAILS.md");

/// Composes a system prompt from parts and an optional channel partial.
///
/// Order: OODA (reasoning) → TOOL_USE → user parts (identity, soul) → GUARDRAILS → channel partial.
/// Compiled-in parts bracket user-editable content: OODA sets the foundation,
/// GUARDRAILS sets the boundaries.
pub fn compose_system_prompt(parts: &[String], channel_partial: Option<&str>) -> String {
    let mut prompt = OODA.to_owned();
    prompt.push_str("\n\n");
    prompt.push_str(TOOL_USE);
    for part in parts {
        prompt.push_str("\n\n");
        prompt.push_str(part);
    }
    prompt.push_str("\n\n");
    prompt.push_str(GUARDRAILS);
    if let Some(partial) = channel_partial {
        prompt.push_str("\n\n");
        prompt.push_str(partial);
    }
    prompt
}
