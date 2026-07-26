// Persona commands: `/caveman`, `/rocky`, `/normal`.
//
// Personas used to be a bespoke "speech mode" mechanism that returned
// `CommandResult::SpeechMode` and stored the prompt text in the TUI. They now
// live in the output-style system (`claurst_core::output_styles`) as ordinary
// built-in styles, so there is ONE place personas are defined. These commands
// are thin wrappers that select the matching output style *persistently*
// (exactly like `/output-style <name>`); `/normal` resets to `default`.
//
// The same personas are also reachable *transiently* by typing the single word
// `caveman` / `rocky` / `normal` inline in a prompt (see
// `claurst_core::keywords`) — that applies to one turn only.

use super::*;
use async_trait::async_trait;

pub struct CavemanCommand;
pub struct RockyCommand;
pub struct NormalCommand;

/// Select an output-style persona persistently and persist it to settings.
///
/// `style` is the output-style name; the reserved name `"default"` clears any
/// active persona (that is what `/normal` does). Mirrors `OutputStyleCommand`
/// so personas and `/output-style` share one persistence path.
fn apply_persona(ctx: &CommandContext, style: &str, confirm: &str) -> CommandResult {
    let selection = (style != "default").then(|| style.to_string());

    let mut new_config = ctx.config.clone();
    new_config.output_style = selection.clone();

    if let Err(err) = save_settings_mutation(|settings| {
        settings.config.output_style = selection.clone();
    }) {
        return CommandResult::Error(format!("Failed to save configuration: {}", err));
    }

    CommandResult::ConfigChangeMessage(new_config, confirm.to_string())
}

// ---- /caveman, /rocky, /normal -------------------------------------------

#[async_trait]
impl SlashCommand for CavemanCommand {
    fn name(&self) -> &str { "caveman" }
    fn description(&self) -> &str { "Caveman persona — why use many token when few token do trick" }
    fn help(&self) -> &str {
        "Usage: /caveman\n\n\
         Switch the output style to the caveman persona (concise, few words) and \
         keep it until you change it. Equivalent to /output-style caveman.\n\n\
         Tip: type `caveman` inline in a prompt to use it for just that one turn.\n\
         Use /normal to deactivate."
    }
    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        apply_persona(ctx, "caveman", "Caveman mode. Full. Oog.")
    }
}

#[async_trait]
impl SlashCommand for RockyCommand {
    fn name(&self) -> &str { "rocky" }
    fn description(&self) -> &str { "Rocky persona — Eridian engineer from Project Hail Mary. Good good good." }
    fn help(&self) -> &str {
        "Usage: /rocky\n\n\
         Switch the output style to the Rocky persona (Project Hail Mary's Eridian \
         engineer) and keep it until you change it. Equivalent to /output-style rocky.\n\n\
         Tip: type `rocky` inline in a prompt to use it for just that one turn.\n\
         Use /normal to deactivate."
    }
    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        apply_persona(ctx, "rocky", "Rocky mode. Full. Good good good.")
    }
}

#[async_trait]
impl SlashCommand for NormalCommand {
    fn name(&self) -> &str { "normal" }
    fn description(&self) -> &str { "Reset the output style / persona to default" }
    fn help(&self) -> &str {
        "Usage: /normal\n\nDeactivate any active persona/output style and return to the default. \
         Equivalent to /output-style default. Typing `normal` inline resets for one turn only."
    }
    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        apply_persona(ctx, "default", "Normal mode.")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn personas_map_to_builtin_output_styles() {
        // The command targets must exist as built-in output styles so
        // /output-style and the inline keywords resolve the same prompt text.
        let styles = claurst_core::output_styles::builtin_styles();
        for name in ["caveman", "rocky"] {
            assert!(
                claurst_core::output_styles::find_style(&styles, name).is_some(),
                "persona command target '{name}' must be a built-in output style"
            );
        }
    }
}
