// SkillTool: load a skill's instructions by name.
//
// Skills come from the session skill registry
// (`claurst_core::skill_discovery::SkillRegistry`): project and global
// `.claurst/skills`, `.agents/skills`, `.claude/skills`, configured
// `skills.paths` / `skills.urls`, and the bundled skills.
//
// The model invokes this tool to expand a skill's prompt inline.
// Supports $ARGUMENTS placeholder substitution.
// Use skill="list" to discover available skills.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use claurst_core::skill_discovery::{RegisteredSkill, SkillRegistry};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::debug;

pub struct SkillTool;

#[derive(Debug, Deserialize)]
struct SkillInput {
    skill: String,
    #[serde(default)]
    args: Option<String>,
}

#[async_trait]
impl Tool for SkillTool {
    // Gates itself: calls `ctx.check_permission_for_path` in `execute()` (#210).
    fn self_gates(&self) -> bool { true }

    fn name(&self) -> &str { "Skill" }

    fn description(&self) -> &str {
        "Load a skill's instructions by name. Skills are listed in the system \
         prompt under <available_skills>; use skill=\"list\" to enumerate them. \
         The skill's instructions are returned for you to follow."
    }

    fn permission_level(&self) -> PermissionLevel { PermissionLevel::ReadOnly }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "Skill name, or \"list\" to enumerate skills"
                },
                "args": {
                    "type": "string",
                    "description": "Arguments passed to the skill — replaces $ARGUMENTS in the template"
                }
            },
            "required": ["skill"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: SkillInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        // Discovery reads the filesystem and may `git clone` configured skill
        // URLs, so it runs on the blocking pool.
        let cwd = ctx.working_dir.clone();
        let skills_config = ctx.config.skills.clone();
        let registry = match tokio::task::spawn_blocking(move || {
            SkillRegistry::discover(&cwd, &skills_config)
        })
        .await
        {
            Ok(registry) => registry,
            Err(e) => return ToolResult::error(format!("Skill discovery failed: {}", e)),
        };

        if params.skill == "list" {
            return list_skills(&registry);
        }
        let skill_name = params.skill.trim_start_matches('/').trim_end_matches(".md");
        debug!(skill = skill_name, "Loading skill");

        let Some(skill) = registry.get(skill_name) else {
            return ToolResult::error(format!(
                "Skill '{}' not found. Use skill=\"list\" to see available skills.",
                skill_name
            ));
        };

        if let Some(path) = skill.source_path() {
            if let Err(e) = ctx.check_permission_for_path(
                self.name(),
                &format!("Load skill {}", skill_name),
                path.to_path_buf(),
                true,
            ) {
                return ToolResult::error(e.to_string());
            }
        }

        let body = skill.expand(params.args.as_deref().unwrap_or(""));
        let body = body.trim();
        if body.is_empty() {
            return ToolResult::error(format!("Skill '{}' expanded to empty content.", skill_name));
        }

        ToolResult::success(wrap_skill_content(skill.name(), skill.base_dir(), body))
    }
}

/// Wrap a skill body for the model. File skills carry their directory as
/// `base_dir` plus a note that relative paths in the body resolve against it;
/// bundled skills have no directory.
fn wrap_skill_content(name: &str, base_dir: Option<&std::path::Path>, body: &str) -> String {
    match base_dir {
        Some(dir) => format!(
            "<skill_content name=\"{name}\" base_dir=\"{dir}\">\n{body}\n\n\
             Relative paths in this skill (for example scripts/ or references/) are \
             relative to the base directory {dir}; use absolute paths built from it in \
             tool calls.\n</skill_content>",
            dir = dir.display()
        ),
        None => format!("<skill_content name=\"{name}\">\n{body}\n</skill_content>"),
    }
}

fn list_skills(registry: &SkillRegistry) -> ToolResult {
    if registry.is_empty() {
        return ToolResult::success(
            "No skills found. Add a skill as .claurst/skills/<name>/SKILL.md \
             (or .agents/skills/, .claude/skills/)."
                .to_string(),
        );
    }
    let lines: Vec<String> = registry
        .skills()
        .iter()
        .map(|skill| match skill {
            RegisteredSkill::Bundled(_) => {
                format!("  {} — {} [bundled]", skill.name(), skill.description())
            }
            RegisteredSkill::Discovered(_) => {
                format!("  {} — {}", skill.name(), skill.description())
            }
        })
        .collect();
    ToolResult::success(format!(
        "Available skills ({}):\n{}",
        lines.len(),
        lines.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    fn ctx_in(dir: &std::path::Path) -> ToolContext {
        use claurst_core::permissions::AutoPermissionHandler;
        let handler = Arc::new(AutoPermissionHandler {
            mode: claurst_core::config::PermissionMode::Default,
        });
        ToolContext {
            working_dir: dir.to_path_buf(),
            permission_mode: claurst_core::config::PermissionMode::Default,
            permission_handler: handler,
            cost_tracker: claurst_core::cost::CostTracker::new(),
            session_id: "test-skill".to_string(),
            file_history: Arc::new(parking_lot::Mutex::new(
                claurst_core::file_history::FileHistory::new(),
            )),
            current_turn: Arc::new(AtomicUsize::new(0)),
            non_interactive: true,
            mcp_manager: None,
            config: claurst_core::config::Config::default(),
            managed_agent_config: None,
            completion_notifier: None,
            pending_permissions: None,
            permission_manager: None,
            user_question_tx: None,
            cancel_token: tokio_util::sync::CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn skill_tool_round_trip_from_project_skill_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let skill_dir = tmp.path().join(".claurst").join("skills").join("deploy-446");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: Deploy\n---\nRun scripts/deploy.sh for $ARGUMENTS",
        )
        .unwrap();

        let ctx = ctx_in(tmp.path());
        let result = SkillTool
            .execute(json!({"skill": "deploy-446", "args": "staging"}), &ctx)
            .await;
        assert!(!result.is_error, "{}", result.content);
        let expected_open = format!(
            "<skill_content name=\"deploy-446\" base_dir=\"{}\">\nRun scripts/deploy.sh for staging\n",
            skill_dir.display()
        );
        assert!(result.content.starts_with(&expected_open), "{}", result.content);
        assert!(result.content.contains("relative to the base directory"));
        assert!(result.content.ends_with("</skill_content>"));

        let listed = SkillTool.execute(json!({"skill": "list"}), &ctx).await;
        assert!(listed.content.contains("deploy-446 — Deploy"), "{}", listed.content);

        let missing = SkillTool.execute(json!({"skill": "no-such-skill-446"}), &ctx).await;
        assert!(missing.is_error);
    }

    #[test]
    fn bundled_skill_content_has_no_base_dir() {
        let wrapped = wrap_skill_content("debug", None, "Body");
        assert_eq!(wrapped, "<skill_content name=\"debug\">\nBody\n</skill_content>");
    }
}
