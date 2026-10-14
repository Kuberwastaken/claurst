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

        ToolResult::success(body.to_string())
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
