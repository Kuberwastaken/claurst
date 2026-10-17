//! Productivity report generation for Claurst.
//!
//! Scans persisted JSONL session transcripts under
//! `~/.claurst/projects/<base64url(cwd)>/<session>.jsonl` (same layout as the
//! `/stats` command), folds them into a [`ProductivityReport`], and renders a
//! shareable Markdown summary. A sidecar cache keeps re-runs cheap by skipping
//! files whose (len, mtime) fingerprint is unchanged.
//!
//! Ported from jcode's `jcode-productivity-core` crate; the PNG dashboard
//! renderer was left out to avoid pulling in resvg/usvg.

mod aggregate;
mod markdown;
mod model;
mod scan;

pub use model::{ProductivityReport, SessionSummary, Tally};

use anyhow::Result;

/// Everything a caller needs to display and share the report.
pub struct ProductivityOutput {
    pub report: ProductivityReport,
    /// Rendered Markdown for the chat transcript.
    pub markdown: String,
}

/// Scan transcripts, compute the report, and render markdown.
pub fn generate() -> Result<ProductivityOutput> {
    let report = compute_report()?;
    let markdown = markdown::render_markdown(&report);
    Ok(ProductivityOutput { report, markdown })
}

/// Scan + aggregate only (no rendering). Useful for tests and JSON export.
pub fn compute_report() -> Result<ProductivityReport> {
    let scan = scan::scan_all();
    Ok(aggregate::build_report(scan))
}

/// Render markdown for a report.
pub fn render_markdown(report: &ProductivityReport) -> String {
    markdown::render_markdown(report)
}

/// Build a report directly from in-memory summaries. Exposed for tests.
pub fn report_from_summaries(summaries: Vec<SessionSummary>) -> ProductivityReport {
    aggregate::report_from_summaries(summaries)
}

#[cfg(test)]
mod tests;
