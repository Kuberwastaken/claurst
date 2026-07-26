// `/review` command (PR review).
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct ReviewCommand;

// ---- /review -------------------------------------------------------------

#[async_trait]
impl SlashCommand for ReviewCommand {
    fn name(&self) -> &str { "review" }
    fn description(&self) -> &str { "Review code changes via LLM and optionally post to GitHub PR" }
    fn help(&self) -> &str {
        "Usage: /review [base-ref]\n\n\
         Runs `git diff <base>...HEAD` (or `git diff --cached` when no base is given),\n\
         sends the diff to the LLM for a structured review, then optionally posts the\n\
         review as a comment to the associated GitHub PR.\n\n\
         GitHub posting requires:\n\
           GITHUB_TOKEN  — a personal access token with repo scope\n\
           CLAUDE_PR_NUMBER — the PR number (auto-detected from `git remote` if absent)\n\n\
         Examples:\n\
           /review            # diff of staged changes\n\
           /review main       # diff from main..HEAD\n\
           /review origin/main"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let base = args.trim();

        // ------------------------------------------------------------------
        // 1. Collect the diff
        // ------------------------------------------------------------------
        let repo_root = claurst_core::git_utils::get_repo_root(&ctx.working_dir)
            .unwrap_or_else(|| ctx.working_dir.clone());

        let diff = if base.is_empty() {
            // No base given — use staged changes; fall back to unstaged if empty.
            let staged = claurst_core::git_utils::get_staged_diff(&repo_root);
            if staged.is_empty() {
                claurst_core::git_utils::get_unstaged_diff(&repo_root)
            } else {
                staged
            }
        } else {
            // Run `git diff <base>...HEAD`
            let out = std::process::Command::new("git")
                .current_dir(&repo_root)
                .args(["diff", &format!("{}...HEAD", base)])
                .output();
            match out {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    return CommandResult::Error(format!(
                        "git diff failed: {}",
                        stderr.trim()
                    ));
                }
                Err(e) => return CommandResult::Error(format!("Failed to run git: {}", e)),
            }
        };

        if diff.is_empty() {
            return CommandResult::Message(
                "No diff found. Stage some changes or provide a base ref (e.g. /review main)."
                    .to_string(),
            );
        }

        // ------------------------------------------------------------------
        // 2. Summarise changed files for the TUI header
        // ------------------------------------------------------------------
        let changed_files: Vec<&str> = diff
            .lines()
            .filter(|l| l.starts_with("diff --git "))
            .filter_map(|l| {
                // "diff --git a/foo/bar.rs b/foo/bar.rs"  -> "foo/bar.rs"
                let parts: Vec<&str> = l.split(' ').collect();
                parts.get(3).map(|p| p.trim_start_matches("b/"))
            })
            .collect();

        let file_summary = if changed_files.is_empty() {
            "Changed files: (unknown)".to_string()
        } else {
            format!(
                "Changed files ({}):\n{}",
                changed_files.len(),
                changed_files
                    .iter()
                    .map(|f| format!("  - {}", f))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };

        // Truncate diff to a sensible size for the LLM (≈ 100 k chars).
        const MAX_DIFF_CHARS: usize = 100_000;
        let diff_for_llm = if diff.len() > MAX_DIFF_CHARS {
            format!(
                "{}\n\n[... diff truncated at {} chars ...]",
                &diff[..MAX_DIFF_CHARS],
                MAX_DIFF_CHARS
            )
        } else {
            diff.clone()
        };

        // ------------------------------------------------------------------
        // 3. Call the LLM for a structured PR review
        // ------------------------------------------------------------------
        let model = ctx.config.effective_model().to_string();
        let provider = match provider_for_config(&ctx.config).await {
            Some(provider) => provider,
            None => {
                return CommandResult::Error(
                    "Cannot initialise provider client for code review.".to_string(),
                );
            }
        };

        let review_prompt = format!(
            "You are a senior software engineer performing a pull-request code review.\n\
             Provide a concise, actionable review of the following diff.\n\n\
             Structure your response as:\n\
             ## Summary\n\
             (1-3 sentences describing what changed)\n\n\
             ## Issues\n\
             (bulleted list: [CRITICAL|MAJOR|MINOR] file:line — description; \
             omit section if none)\n\n\
             ## Suggestions\n\
             (bulleted list of optional improvements; omit section if none)\n\n\
             ## Verdict\n\
             APPROVE / REQUEST_CHANGES / COMMENT — one line with brief rationale\n\n\
             ---\n\
             {}\n\n\
             ```diff\n\
             {}\n\
             ```",
            file_summary, diff_for_llm
        );

        let request = claurst_api::ProviderRequest {
            model,
            messages: vec![Message::user(review_prompt)],
            system_prompt: Some(claurst_api::SystemPrompt::Text(
                "You are a thorough, constructive code reviewer. \
                 Be concise but precise. Focus on correctness, security, and maintainability."
                    .to_string(),
            )),
            tools: vec![],
            max_tokens: 4096,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: vec![],
            thinking: None,
            provider_options: serde_json::Value::Object(Default::default()),
        };

        let review_text = match provider.create_message(request).await {
            Err(e) => {
                return CommandResult::Error(format!("LLM call failed: {}", e));
            }
            Ok(response) => {
                let text = text_from_content_blocks(&response.content);
                if text.trim().is_empty() {
                    return CommandResult::Error("LLM returned an empty review.".to_string());
                }
                text
            }
        };

        // ------------------------------------------------------------------
        // 4. Optionally post to GitHub PR
        // ------------------------------------------------------------------
        let github_token = std::env::var("GITHUB_TOKEN").ok();
        let mut github_post_result: Option<String> = None;

        if let Some(ref token) = github_token {
            // Determine PR number
            let pr_number: Option<u64> = std::env::var("CLAUDE_PR_NUMBER")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .or_else(|| detect_pr_number_from_git(&repo_root));

            if let Some(pr_num) = pr_number {
                // Determine owner/repo from git remote
                if let Some((owner, repo)) = detect_github_owner_repo(&repo_root) {
                    let comment_body = format!(
                        "## Claurst Code Review\n\n{}\n\n---\n*Generated by [Claurst](https://claude.ai/claude-code)*",
                        review_text
                    );

                    let url = format!(
                        "https://api.github.com/repos/{}/{}/issues/{}/comments",
                        owner, repo, pr_num
                    );

                    let http = reqwest::Client::new();
                    let post_result = http
                        .post(&url)
                        .header("Authorization", format!("Bearer {}", token))
                        .header("User-Agent", "claurst/1.0")
                        .header("Accept", "application/vnd.github+json")
                        .json(&serde_json::json!({ "body": comment_body }))
                        .send()
                        .await;

                    match post_result {
                        Ok(resp) if resp.status().is_success() => {
                            github_post_result = Some(format!(
                                "\nPosted review comment to PR #{} ({}/{}).",
                                pr_num, owner, repo
                            ));
                        }
                        Ok(resp) => {
                            let status = resp.status().as_u16();
                            let body = resp.text().await.unwrap_or_default();
                            github_post_result = Some(format!(
                                "\nGitHub API returned {}: {}",
                                status, body
                            ));
                        }
                        Err(e) => {
                            github_post_result =
                                Some(format!("\nFailed to post to GitHub: {}", e));
                        }
                    }
                } else {
                    github_post_result = Some(
                        "\n(Could not detect GitHub owner/repo from git remote — \
                         review not posted.)"
                            .to_string(),
                    );
                }
            } else {
                github_post_result = Some(
                    "\n(GITHUB_TOKEN set but no PR number found. \
                     Set CLAUDE_PR_NUMBER=<n> to post the review.)"
                        .to_string(),
                );
            }
        }

        // ------------------------------------------------------------------
        // 5. Compose and return the final output
        // ------------------------------------------------------------------
        let mut output = format!("## Code Review\n\n{}\n\n{}", file_summary, review_text);

        if let Some(ref note) = github_post_result {
            output.push_str(note);
        }

        CommandResult::Message(output)
    }
}

/// Try to detect the PR number from the GitHub API via `gh` CLI, then fall
/// back to parsing the upstream tracking branch name (e.g. `pr/42/head`).
fn detect_pr_number_from_git(repo_root: &std::path::Path) -> Option<u64> {
    // Attempt `gh pr view --json number -q .number`
    let out = std::process::Command::new("gh")
        .current_dir(repo_root)
        .args(["pr", "view", "--json", "number", "-q", ".number"])
        .output()
        .ok()?;

    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout);
        return s.trim().parse::<u64>().ok();
    }

    // Fallback: look at the upstream tracking ref for a pattern like
    // `refs/pull/42/head` or branch name `pr/42`.
    let tracking = std::process::Command::new("git")
        .current_dir(repo_root)
        .args(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    // Pattern: "origin/pr/42" or "refs/pull/42/head"
    for segment in tracking.split('/') {
        if let Ok(n) = segment.parse::<u64>() {
            return Some(n);
        }
    }

    None
}

/// Parse `origin` remote URL to extract GitHub owner and repo name.
/// Handles both HTTPS (`https://github.com/owner/repo.git`) and
/// SSH (`git@github.com:owner/repo.git`) formats.
fn detect_github_owner_repo(repo_root: &std::path::Path) -> Option<(String, String)> {
    let remote_url = std::process::Command::new("git")
        .current_dir(repo_root)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())?;

    parse_github_remote_url(&remote_url)
}

fn parse_github_remote_url(url: &str) -> Option<(String, String)> {
    // HTTPS: https://github.com/owner/repo.git  or  https://github.com/owner/repo
    if let Some(rest) = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
    {
        let clean = rest.trim_end_matches(".git");
        let mut parts = clean.splitn(2, '/');
        let owner = parts.next()?.to_string();
        let repo = parts.next()?.to_string();
        return Some((owner, repo));
    }

    // SSH: git@github.com:owner/repo.git
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        let clean = rest.trim_end_matches(".git");
        let mut parts = clean.splitn(2, '/');
        let owner = parts.next()?.to_string();
        let repo = parts.next()?.to_string();
        return Some((owner, repo));
    }

    None
}
