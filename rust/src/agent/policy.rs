//! Declarative command policy guardrails loaded via `sharky agent --policy <file>`.

use std::path::Path;

use regex_lite::Regex;

use super::json::{Value, parse_line};
use super::protocol::{ExecTarget, SandboxSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyAction {
    Deny,
    RequireApproval,
    ForceSandbox,
}

#[derive(Debug, Clone)]
pub(crate) struct PolicyRule {
    pub(crate) id: String,
    regex: Regex,
    pub(crate) action: PolicyAction,
    pub(crate) message: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Policy {
    rules: Vec<PolicyRule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PolicyDecision {
    Allow,
    ForceSandbox,
    Deny { id: String, message: String },
    RequireApproval { id: String, message: String },
}

impl Policy {
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path)
            .map_err(|err| format!("cannot read policy file {}: {err}", path.display()))?;
        let doc = parse_line(&bytes)
            .map_err(|err| format!("invalid policy JSON {}: {err}", path.display()))?;
        let Some(rules_val) = doc.get("rules").and_then(Value::as_array) else {
            return Err(format!("policy file {} must contain a 'rules' array", path.display()));
        };

        let mut rules = Vec::with_capacity(rules_val.len());
        for (idx, item) in rules_val.iter().enumerate() {
            if item.as_object().is_none() {
                return Err(format!("policy rule #{idx} must be an object"));
            }
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("rule-{idx}"));
            let Some(pattern) = item.get("pattern").and_then(Value::as_str) else {
                return Err(format!("policy rule '{id}' requires string 'pattern'"));
            };
            let regex = Regex::new(pattern)
                .map_err(|err| format!("policy rule '{id}' invalid regex '{pattern}': {err}"))?;
            let Some(action_str) = item.get("action").and_then(Value::as_str) else {
                return Err(format!("policy rule '{id}' requires string 'action'"));
            };
            let action = match action_str {
                "deny" => PolicyAction::Deny,
                "require_approval" => PolicyAction::RequireApproval,
                "force_sandbox" => PolicyAction::ForceSandbox,
                other => {
                    return Err(format!(
                        "policy rule '{id}' unknown action '{other}'; expected deny, require_approval, or force_sandbox"
                    ));
                }
            };
            let message = item
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(match action {
                    PolicyAction::Deny => "command denied by policy",
                    PolicyAction::RequireApproval => "command requires explicit approval",
                    PolicyAction::ForceSandbox => "sandbox forced by policy",
                })
                .to_string();
            rules.push(PolicyRule {
                id,
                regex,
                action,
                message,
            });
        }

        Ok(Self { rules })
    }

    pub(crate) fn evaluate_target(&self, target: &ExecTarget) -> PolicyDecision {
        let owned;
        let text = match target {
            ExecTarget::Command(cmd) => cmd.as_str(),
            ExecTarget::Argv(argv) => {
                owned = argv.join(" ");
                owned.as_str()
            }
        };
        let mut forced = false;
        for rule in &self.rules {
            if !rule.regex.is_match(text) {
                continue;
            }
            match rule.action {
                PolicyAction::Deny => {
                    return PolicyDecision::Deny {
                        id: rule.id.clone(),
                        message: rule.message.clone(),
                    };
                }
                PolicyAction::RequireApproval => {
                    return PolicyDecision::RequireApproval {
                        id: rule.id.clone(),
                        message: rule.message.clone(),
                    };
                }
                PolicyAction::ForceSandbox => {
                    forced = true;
                }
            }
        }
        if forced {
            PolicyDecision::ForceSandbox
        } else {
            PolicyDecision::Allow
        }
    }
}

pub(crate) fn default_forced_sandbox() -> SandboxSpec {
    SandboxSpec {
        no_network: true,
        read_only_root: false,
        max_file_bytes: Some(64 * 1024 * 1024),
        max_procs: None,
        cpu_seconds: Some(60),
    }
}
