#[test]
fn test_workflows_active_on_overrides_strong_model() {
    let mut config = crate::config::Config::default();
    config.llm.model = "claude-opus-4".to_string();
    config.workflow.enabled = "on".to_string();
    assert!(crate::workflow::workflows_active(&config));
}

#[test]
fn test_workflows_active_off_overrides_weak_model() {
    let mut config = crate::config::Config::default();
    config.llm.model = "llama-3.3-70b".to_string();
    config.workflow.enabled = "off".to_string();
    assert!(!crate::workflow::workflows_active(&config));
}

#[test]
fn test_workflows_active_auto_uses_model_heuristic() {
    let mut config = crate::config::Config::default();
    config.workflow.enabled = "auto".to_string();

    // Opus → no workflows
    config.llm.model = "claude-opus-4".to_string();
    assert!(!crate::workflow::workflows_active(&config));

    // Sonnet → no workflows (all Claude models skip)
    config.llm.model = "claude-sonnet-4".to_string();
    assert!(!crate::workflow::workflows_active(&config));

    // GPT-4o → workflows
    config.llm.model = "gpt-4o".to_string();
    assert!(crate::workflow::workflows_active(&config));

    // Unknown → workflows
    config.llm.model = "custom-model".to_string();
    assert!(crate::workflow::workflows_active(&config));
}

#[test]
fn test_workflows_active_default_is_auto() {
    let config = crate::config::Config::default();
    assert_eq!(config.workflow.enabled, "auto");
}
