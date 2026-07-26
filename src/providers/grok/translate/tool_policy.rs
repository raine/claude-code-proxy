use std::collections::{HashMap, HashSet};

use super::request::{GrokResponsesRequest, GrokToolChoice};

/// Response-side tool permissions derived from the final Grok wire request.
///
/// Request translation filters deferred and non-direct tools before this policy is built. Keeping
/// the policy beside the reducer prevents an upstream response from widening those permissions.
#[derive(Clone, Debug)]
pub(crate) struct ToolCallPolicy {
    function_rule: FunctionRule,
    hosted_rule: HostedRule,
    hosted_limits: HashMap<String, u64>,
    terminal_requirement: TerminalRequirement,
    single_tool_only: bool,
}

#[derive(Clone, Debug)]
enum FunctionRule {
    Permissive,
    Forbidden,
    Declared(HashSet<String>),
    Named(String),
}

#[derive(Clone, Debug)]
enum HostedRule {
    Permissive,
    Forbidden,
    Declared(HashSet<String>),
}

#[derive(Clone, Debug)]
enum TerminalRequirement {
    None,
    AnyTool,
    NamedFunction(String),
}

impl Default for ToolCallPolicy {
    fn default() -> Self {
        Self::permissive()
    }
}

impl ToolCallPolicy {
    /// Compatibility policy for lower-level helpers that have no associated request. Production
    /// paths must use [`Self::from_request`].
    pub(crate) fn permissive() -> Self {
        Self {
            function_rule: FunctionRule::Permissive,
            hosted_rule: HostedRule::Permissive,
            hosted_limits: HashMap::new(),
            terminal_requirement: TerminalRequirement::None,
            single_tool_only: false,
        }
    }

    pub(crate) fn from_request(request: &GrokResponsesRequest) -> Self {
        let mut declared_functions = HashSet::new();
        let mut declared_hosted = HashSet::new();
        let mut hosted_limits = HashMap::new();
        for tool in request.tools.iter().flatten() {
            match tool.kind.as_str() {
                "function" => {
                    if let Some(name) = &tool.name {
                        declared_functions.insert(name.clone());
                    }
                }
                "web_search" | "x_search" => {
                    let kind = tool.kind.clone();
                    declared_hosted.insert(kind.clone());
                    if let Some(limit) = tool.max_uses {
                        hosted_limits.insert(kind, limit);
                    }
                }
                _ => {}
            }
        }
        let single_tool_only = request.parallel_tool_calls == Some(false);

        match request.tool_choice.as_ref() {
            Some(GrokToolChoice::None(_)) => Self {
                function_rule: FunctionRule::Forbidden,
                hosted_rule: HostedRule::Forbidden,
                hosted_limits,
                terminal_requirement: TerminalRequirement::None,
                single_tool_only,
            },
            Some(GrokToolChoice::Function { name, .. }) => Self {
                function_rule: if declared_functions.contains(name) {
                    FunctionRule::Named(name.clone())
                } else {
                    FunctionRule::Forbidden
                },
                hosted_rule: HostedRule::Forbidden,
                hosted_limits,
                terminal_requirement: TerminalRequirement::NamedFunction(name.clone()),
                single_tool_only,
            },
            Some(GrokToolChoice::Required(_)) => Self {
                function_rule: FunctionRule::Declared(declared_functions),
                hosted_rule: HostedRule::Declared(declared_hosted),
                hosted_limits,
                terminal_requirement: TerminalRequirement::AnyTool,
                single_tool_only,
            },
            Some(GrokToolChoice::Auto(_)) | None => Self {
                function_rule: FunctionRule::Declared(declared_functions),
                hosted_rule: HostedRule::Declared(declared_hosted),
                hosted_limits,
                terminal_requirement: TerminalRequirement::None,
                single_tool_only,
            },
        }
    }

    pub(crate) fn validate_function_call(
        &self,
        name: &str,
        prior_tool_calls: usize,
    ) -> anyhow::Result<()> {
        match &self.function_rule {
            FunctionRule::Permissive => {}
            FunctionRule::Forbidden => {
                anyhow::bail!("Grok returned function tool {name:?} while tool_choice forbids it")
            }
            FunctionRule::Declared(names) if names.contains(name) => {}
            FunctionRule::Declared(_) => {
                anyhow::bail!("Grok returned undeclared function tool {name:?}")
            }
            FunctionRule::Named(required) if required == name => {}
            FunctionRule::Named(required) => anyhow::bail!(
                "Grok returned function tool {name:?} while tool_choice requires {required:?}"
            ),
        }
        self.validate_parallel_limit(prior_tool_calls)
    }

    pub(crate) fn validate_hosted_call(
        &self,
        kind: &str,
        prior_tool_calls: usize,
        prior_kind_calls: usize,
    ) -> anyhow::Result<()> {
        match &self.hosted_rule {
            HostedRule::Permissive => {}
            HostedRule::Forbidden => {
                anyhow::bail!("Grok returned hosted tool {kind:?} while tool_choice forbids it")
            }
            HostedRule::Declared(kinds) if kinds.contains(kind) => {}
            HostedRule::Declared(_) => {
                anyhow::bail!("Grok returned undeclared hosted tool {kind:?}")
            }
        }
        if self
            .hosted_limits
            .get(kind)
            .is_some_and(|limit| u64::try_from(prior_kind_calls).unwrap_or(u64::MAX) >= *limit)
        {
            anyhow::bail!("Grok returned hosted tool {kind:?} more times than max_uses allows")
        }
        self.validate_parallel_limit(prior_tool_calls)
    }

    pub(crate) fn validate_success_terminal(
        &self,
        completed_function_calls: usize,
        completed_visible_hosted_calls: usize,
    ) -> anyhow::Result<()> {
        match &self.terminal_requirement {
            TerminalRequirement::None => Ok(()),
            TerminalRequirement::AnyTool
                if completed_function_calls > 0 || completed_visible_hosted_calls > 0 =>
            {
                Ok(())
            }
            TerminalRequirement::AnyTool => {
                anyhow::bail!("Grok completed without the tool call required by tool_choice")
            }
            TerminalRequirement::NamedFunction(_) if completed_function_calls > 0 => Ok(()),
            TerminalRequirement::NamedFunction(name) => {
                anyhow::bail!("Grok completed without required function tool {name:?}")
            }
        }
    }

    fn validate_parallel_limit(&self, prior_tool_calls: usize) -> anyhow::Result<()> {
        if self.single_tool_only && prior_tool_calls > 0 {
            anyhow::bail!("Grok returned multiple tool calls while parallel_tool_calls is false")
        }
        Ok(())
    }
}
