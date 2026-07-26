// `/ultrareview` command.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct UltrareviewCommand;

// ---- /ultrareview --------------------------------------------------------

#[async_trait]
impl SlashCommand for UltrareviewCommand {
    fn name(&self) -> &str { "ultrareview" }
    fn description(&self) -> &str { "Run an exhaustive multi-dimensional code review" }
    fn help(&self) -> &str {
        "Usage: /ultrareview [path]\n\n\
         Runs a comprehensive code review that goes beyond /review and\n\
         /security-review. Covers: security (OWASP Top 10), performance,\n\
         maintainability, test coverage, error handling, API design,\n\
         documentation, accessibility, and architectural concerns.\n\
         Each finding is tagged by category and severity."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let target = if args.trim().is_empty() {
            ctx.working_dir.display().to_string()
        } else {
            args.trim().to_string()
        };

        CommandResult::UserMessage(format!(
            "Please perform an **ultra-comprehensive code review** of the code in `{target}`.\n\n\
             This review must go beyond a standard review and cover ALL of the following dimensions:\n\n\
             ## 1. Security (OWASP Top 10 + extras)\n\
             - Injection vulnerabilities (SQL, command, LDAP, XSS, SSTI, CRLF)\n\
             - Broken authentication / session management\n\
             - Sensitive data exposure (secrets, PII, tokens in logs or source)\n\
             - XML/JSON External Entity (XXE) processing\n\
             - Broken access control and privilege escalation paths\n\
             - Security misconfiguration (default creds, open ports, verbose errors)\n\
             - Cross-site scripting (Stored, Reflected, DOM-based)\n\
             - Insecure deserialization\n\
             - Using components with known vulnerabilities (outdated deps)\n\
             - Insufficient logging and monitoring\n\
             - Path traversal and file inclusion\n\
             - Race conditions, TOCTOU, deadlocks\n\
             - Cryptographic weaknesses (weak algorithms, key reuse, bad IV)\n\
             - Supply chain / dependency confusion risks\n\n\
             ## 2. Performance\n\
             - Algorithmic complexity: O(n²) or worse in hot paths\n\
             - Unnecessary allocations, copies, or clones\n\
             - Database N+1 query patterns\n\
             - Missing indexes on frequently queried fields\n\
             - Blocking I/O in async contexts\n\
             - Unbounded loops or recursion\n\
             - Memory leaks or resource leaks (file handles, sockets)\n\
             - Caching opportunities\n\n\
             ## 3. Maintainability & Code Quality\n\
             - Functions / methods exceeding 50 lines\n\
             - Deep nesting (>4 levels)\n\
             - Duplicated logic (DRY violations)\n\
             - Magic numbers and strings without named constants\n\
             - Misleading names (variables, functions, types)\n\
             - Dead code and unused imports\n\
             - Overly complex conditionals\n\
             - Coupling: tight coupling between unrelated modules\n\n\
             ## 4. Error Handling\n\
             - Swallowed errors (empty catch blocks, `unwrap()` without context)\n\
             - Panic-able paths in library code\n\
             - Missing input validation at trust boundaries\n\
             - Unclear error messages that hinder debugging\n\
             - Error type inconsistency across the codebase\n\n\
             ## 5. Test Coverage\n\
             - Missing unit tests for critical logic\n\
             - Missing integration tests for external boundaries\n\
             - Tests with no assertions\n\
             - Tests that are brittle (time-dependent, order-dependent)\n\
             - Missing negative / edge-case tests\n\
             - Mocking strategy concerns\n\n\
             ## 6. API Design\n\
             - Unclear or inconsistent naming conventions\n\
             - Functions with too many parameters (>5)\n\
             - Mutable global state\n\
             - Missing or incorrect use of visibility modifiers\n\
             - Breaking changes risk in public interfaces\n\
             - Lack of builder or fluent patterns where appropriate\n\n\
             ## 7. Documentation\n\
             - Missing doc comments on public items\n\
             - Outdated or misleading comments\n\
             - Undocumented panics, unsafe blocks, or invariants\n\
             - Missing README or high-level architectural overview\n\n\
             ## 8. Architectural Concerns\n\
             - Single Responsibility Principle violations\n\
             - Circular dependencies\n\
             - Missing abstraction layers\n\
             - Hardcoded configuration that should be externalised\n\
             - Observability gaps (missing tracing, metrics, structured logs)\n\n\
             ## Output Format\n\
             For **every** finding, provide:\n\
             - **Category** (from the dimensions above)\n\
             - **Severity**: Critical / High / Medium / Low / Informational\n\
             - **File** and **line number** (if applicable)\n\
             - **Description** of the issue\n\
             - **Impact**: what can go wrong\n\
             - **Recommended fix** with a code snippet where helpful\n\n\
             Start by reading the main source files, dependency manifests, and any CI/CD configuration.\n\
             Group findings by severity (Critical first). Conclude with a prioritised action plan.",
            target = target,
        ))
    }
}
