pub enum PreGuardrailResult {
    Passed,
    Info(String),
    Warning(String),
    Error(String),
}

pub trait PreGuardrail: Send + Sync {
    fn name(&self) -> &str;
    fn check(&self, content: &str) -> PreGuardrailResult;
}

pub enum PostGuardrailAction {
    Passed,
    Repaired(String),
    NeedsRetry,
}

pub trait PostGuardrail: Send + Sync {
    fn name(&self) -> &str;
    fn check(&self, content: &str) -> PostGuardrailAction;
}

pub struct GuardrailChain {
    pre: Vec<Box<dyn PreGuardrail>>,
    post: Vec<Box<dyn PostGuardrail>>,
}

impl GuardrailChain {
    pub fn new() -> Self {
        Self {
            pre: Vec::new(),
            post: Vec::new(),
        }
    }

    pub fn add_pre(&mut self, guardrail: Box<dyn PreGuardrail>) {
        self.pre.push(guardrail);
    }

    pub fn add_post(&mut self, guardrail: Box<dyn PostGuardrail>) {
        self.post.push(guardrail);
    }

    pub fn run_pre(&self, content: &str) -> Result<(), String> {
        for g in &self.pre {
            match g.check(content) {
                PreGuardrailResult::Passed => {}
                PreGuardrailResult::Info(msg) => log::info!("[guardrail:{}] {}", g.name(), msg),
                PreGuardrailResult::Warning(msg) => log::warn!("[guardrail:{}] {}", g.name(), msg),
                PreGuardrailResult::Error(msg) => {
                    log::error!("[guardrail:{}] {}", g.name(), msg);
                    return Err(msg);
                }
            }
        }
        Ok(())
    }

    pub fn run_post(&self, content: &str) -> PostGuardrailAction {
        let mut current = content.to_string();
        for g in &self.post {
            match g.check(&current) {
                PostGuardrailAction::Passed => {}
                PostGuardrailAction::Repaired(repaired) => {
                    log::info!("[guardrail:{}] repaired response", g.name());
                    current = repaired;
                }
                PostGuardrailAction::NeedsRetry => {
                    log::warn!("[guardrail:{}] retry needed", g.name());
                    return PostGuardrailAction::NeedsRetry;
                }
            }
        }
        if current != content {
            PostGuardrailAction::Repaired(current)
        } else {
            PostGuardrailAction::Passed
        }
    }
}

pub struct Convention {
    pub name: String,
    pub guidance: String,
    pub pre_guardrails: Vec<Box<dyn PreGuardrail>>,
    pub post_guardrails: Vec<Box<dyn PostGuardrail>>,
}

pub struct ConventionRegistry {
    conventions: Vec<Convention>,
}

impl ConventionRegistry {
    pub fn new() -> Self {
        Self {
            conventions: Vec::new(),
        }
    }

    pub fn register(&mut self, convention: Convention) {
        self.conventions.push(convention);
    }

    pub fn compose_guidance(&self) -> String {
        if self.conventions.is_empty() {
            return String::new();
        }
        self.conventions
            .iter()
            .map(|c| format!("### {}\n\n{}", c.name, c.guidance))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn build_guardrail_chain(self) -> GuardrailChain {
        let mut chain = GuardrailChain::new();
        for conv in self.conventions {
            for g in conv.pre_guardrails {
                chain.add_pre(g);
            }
            for g in conv.post_guardrails {
                chain.add_post(g);
            }
        }
        chain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Post guardrail mocks ---

    struct PassGuardrail;

    impl PostGuardrail for PassGuardrail {
        fn name(&self) -> &str {
            "pass"
        }
        fn check(&self, _content: &str) -> PostGuardrailAction {
            PostGuardrailAction::Passed
        }
    }

    struct RepairGuardrail(String);

    impl PostGuardrail for RepairGuardrail {
        fn name(&self) -> &str {
            "repair"
        }
        fn check(&self, content: &str) -> PostGuardrailAction {
            PostGuardrailAction::Repaired(format!("{}{}", content, self.0))
        }
    }

    struct RetryGuardrail;

    impl PostGuardrail for RetryGuardrail {
        fn name(&self) -> &str {
            "retry"
        }
        fn check(&self, _content: &str) -> PostGuardrailAction {
            PostGuardrailAction::NeedsRetry
        }
    }

    // --- Pre guardrail mocks ---

    struct PrePassGuardrail;

    impl PreGuardrail for PrePassGuardrail {
        fn name(&self) -> &str {
            "pre_pass"
        }
        fn check(&self, _content: &str) -> PreGuardrailResult {
            PreGuardrailResult::Passed
        }
    }

    struct PreWarnGuardrail(String);

    impl PreGuardrail for PreWarnGuardrail {
        fn name(&self) -> &str {
            "pre_warn"
        }
        fn check(&self, _content: &str) -> PreGuardrailResult {
            PreGuardrailResult::Warning(self.0.clone())
        }
    }

    struct PreErrorGuardrail(String);

    impl PreGuardrail for PreErrorGuardrail {
        fn name(&self) -> &str {
            "pre_error"
        }
        fn check(&self, _content: &str) -> PreGuardrailResult {
            PreGuardrailResult::Error(self.0.clone())
        }
    }

    // --- Post chain tests ---

    #[test]
    fn test_post_chain_all_pass() {
        let mut chain = GuardrailChain::new();
        chain.add_post(Box::new(PassGuardrail));
        chain.add_post(Box::new(PassGuardrail));
        match chain.run_post("hello") {
            PostGuardrailAction::Passed => {}
            other => panic!("expected Passed, got {:?}", action_name(&other)),
        }
    }

    #[test]
    fn test_post_chain_repair_cascades() {
        let mut chain = GuardrailChain::new();
        chain.add_post(Box::new(RepairGuardrail("X".to_string())));
        chain.add_post(Box::new(PassGuardrail));
        match chain.run_post("hello") {
            PostGuardrailAction::Repaired(s) => {
                assert!(s.contains("X"), "repaired content should contain X");
                assert_eq!(s, "helloX");
            }
            other => panic!("expected Repaired, got {:?}", action_name(&other)),
        }
    }

    #[test]
    fn test_post_chain_retry_stops() {
        // RetryGuardrail first, PassGuardrail second -> NeedsRetry
        let mut chain = GuardrailChain::new();
        chain.add_post(Box::new(RetryGuardrail));
        chain.add_post(Box::new(PassGuardrail));
        match chain.run_post("hello") {
            PostGuardrailAction::NeedsRetry => {}
            other => panic!("expected NeedsRetry, got {:?}", action_name(&other)),
        }

        // Even if second is also RetryGuardrail, result is still NeedsRetry from first
        let mut chain2 = GuardrailChain::new();
        chain2.add_post(Box::new(RetryGuardrail));
        chain2.add_post(Box::new(RetryGuardrail));
        match chain2.run_post("hello") {
            PostGuardrailAction::NeedsRetry => {}
            other => panic!("expected NeedsRetry, got {:?}", action_name(&other)),
        }
    }

    #[test]
    fn test_post_chain_repair_then_retry() {
        let mut chain = GuardrailChain::new();
        chain.add_post(Box::new(RepairGuardrail("X".to_string())));
        chain.add_post(Box::new(RetryGuardrail));
        match chain.run_post("hello") {
            PostGuardrailAction::NeedsRetry => {}
            other => panic!("expected NeedsRetry, got {:?}", action_name(&other)),
        }
    }

    // --- Pre chain tests ---

    #[test]
    fn test_pre_chain_all_pass() {
        let mut chain = GuardrailChain::new();
        chain.add_pre(Box::new(PrePassGuardrail));
        chain.add_pre(Box::new(PrePassGuardrail));
        assert!(chain.run_pre("hello").is_ok());
    }

    #[test]
    fn test_pre_chain_error_blocks() {
        let mut chain = GuardrailChain::new();
        chain.add_pre(Box::new(PrePassGuardrail));
        chain.add_pre(Box::new(PreErrorGuardrail("blocked".to_string())));
        let result = chain.run_pre("hello");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "blocked");
    }

    #[test]
    fn test_pre_chain_warning_continues() {
        let mut chain = GuardrailChain::new();
        chain.add_pre(Box::new(PreWarnGuardrail("caution".to_string())));
        chain.add_pre(Box::new(PrePassGuardrail));
        assert!(chain.run_pre("hello").is_ok());
    }

    // Helper for debug output in panics
    fn action_name(action: &PostGuardrailAction) -> &'static str {
        match action {
            PostGuardrailAction::Passed => "Passed",
            PostGuardrailAction::Repaired(_) => "Repaired",
            PostGuardrailAction::NeedsRetry => "NeedsRetry",
        }
    }

    // --- Convention registry tests ---

    #[test]
    fn test_registry_compose_guidance() {
        let mut registry = ConventionRegistry::new();
        registry.register(Convention {
            name: "Alpha".into(),
            guidance: "Do alpha things.".into(),
            pre_guardrails: vec![],
            post_guardrails: vec![],
        });
        registry.register(Convention {
            name: "Beta".into(),
            guidance: "Do beta things.".into(),
            pre_guardrails: vec![],
            post_guardrails: vec![],
        });
        let guidance = registry.compose_guidance();
        assert!(guidance.contains("### Alpha"));
        assert!(guidance.contains("Do alpha things."));
        assert!(guidance.contains("### Beta"));
        assert!(guidance.contains("Do beta things."));
    }

    #[test]
    fn test_registry_compose_guidance_empty() {
        let registry = ConventionRegistry::new();
        assert_eq!(registry.compose_guidance(), "");
    }

    #[test]
    fn test_registry_build_chain_collects_post() {
        let mut registry = ConventionRegistry::new();
        registry.register(Convention {
            name: "Test".into(),
            guidance: "".into(),
            pre_guardrails: vec![],
            post_guardrails: vec![Box::new(RepairGuardrail("_fixed".into()))],
        });
        let chain = registry.build_guardrail_chain();
        match chain.run_post("input") {
            PostGuardrailAction::Repaired(s) => assert_eq!(s, "input_fixed"),
            other => panic!(
                "Expected Repaired, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn test_registry_build_chain_collects_pre() {
        let mut registry = ConventionRegistry::new();
        registry.register(Convention {
            name: "Test".into(),
            guidance: "".into(),
            pre_guardrails: vec![Box::new(PreErrorGuardrail("blocked".into()))],
            post_guardrails: vec![],
        });
        let chain = registry.build_guardrail_chain();
        assert!(chain.run_pre("input").is_err());
    }

    #[test]
    fn test_registry_build_chain_empty() {
        let registry = ConventionRegistry::new();
        let chain = registry.build_guardrail_chain();
        assert!(matches!(
            chain.run_post("input"),
            PostGuardrailAction::Passed
        ));
        assert!(chain.run_pre("input").is_ok());
    }
}
