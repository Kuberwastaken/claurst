// System-prompt assembly for the query loop.
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use crate::*;

/// Build the system prompt from config.
///
/// Delegates to `claurst_core::system_prompt::build_system_prompt` so that all
/// default content (capabilities, safety guidelines, dynamic-boundary marker,
/// etc.) is assembled in one place.  The `QueryConfig` fields map directly to
/// `SystemPromptOptions`:
///
/// - `system_prompt`        → `custom_system_prompt` (added to cacheable block)
/// - `append_system_prompt` → `append_system_prompt` (added after boundary)
pub(crate) fn build_system_prompt(config: &QueryConfig) -> SystemPrompt {
    use claurst_core::system_prompt::SystemPromptOptions;

    let opts = SystemPromptOptions {
        custom_system_prompt: config.system_prompt.clone(),
        append_system_prompt: config.append_system_prompt.clone(),
        // All other fields use sensible defaults:
        // - prefix:                auto-detect from env
        // - memory_content:        empty (callers inject via append if needed)
        // - replace_system_prompt: false (additive mode)
        // - coordinator_mode:      false
        output_style: config.output_style,
        custom_output_style_prompt: config.output_style_prompt.clone(),
        working_directory: config.working_directory.clone(),
        // Forward the session's enabled tool set so per-tool guideline blocks
        // are only emitted for tools that are actually loaded (issue #233).
        enabled_tools: config.enabled_tools.clone(),
        ..Default::default()
    };

    let text = claurst_core::system_prompt::build_system_prompt(&opts);
    SystemPrompt::Text(text)
}
