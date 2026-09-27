//! Persistent memory store: entries, categories, graph storage, and scoring.
//!
//! Ported from jcode's `jcode-memory-types` crate, trimmed to the parts the
//! claurst memory sidecar consumes: typed entries with trust/category/confidence
//! decay, graph-backed project/global persistence, lexical search, and
//! prompt formatting. Embedding vectors, skill-synthetic entries, and Jev
//! recall plumbing are intentionally out of scope for this port.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

/// Trust levels for memories.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TrustLevel {
    /// User explicitly stated this.
    High,
    /// Observed from user behavior.
    #[default]
    Medium,
    /// Inferred by the agent.
    Low,
}

/// A reinforcement breadcrumb tracking when/where a memory was reinforced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reinforcement {
    pub session_id: String,
    pub message_index: usize,
    pub timestamp: DateTime<Utc>,
}

/// Memory category. `Custom` keeps jcode's open set while the four canonical
/// kinds drive scoring and prompt section ordering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum MemoryCategory {
    Fact,
    Preference,
    Entity,
    Correction,
    Custom(String),
}

impl std::fmt::Display for MemoryCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryCategory::Fact => write!(f, "fact"),
            MemoryCategory::Preference => write!(f, "preference"),
            MemoryCategory::Entity => write!(f, "entity"),
            MemoryCategory::Correction => write!(f, "correction"),
            MemoryCategory::Custom(s) => write!(f, "{}", s),
        }
    }
}

impl MemoryCategory {
    /// Parse a category string from LLM extraction output.
    /// Maps legacy/incorrect category names to the correct variant and avoids
    /// blindly defaulting to Fact.
    pub fn from_extracted(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "fact" | "facts" => MemoryCategory::Fact,
            "preference" | "preferences" | "pref" => MemoryCategory::Preference,
            "correction" | "corrections" | "fix" | "bug" => MemoryCategory::Correction,
            "entity" | "entities" => MemoryCategory::Entity,
            "observation" | "lesson" | "learning" => MemoryCategory::Fact,
            _ => MemoryCategory::Fact,
        }
    }
}

/// A single memory entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub category: MemoryCategory,
    pub content: String,
    pub tags: Vec<String>,
    /// Pre-normalized lowercase search text for content + tags.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub search_text: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub access_count: u32,
    pub source: Option<String>,
    /// Trust level for this memory.
    #[serde(default)]
    pub trust: TrustLevel,
    /// Consolidation strength (how many times this was reinforced).
    #[serde(default)]
    pub strength: u32,
    /// Whether this memory is active or superseded.
    #[serde(default = "default_active")]
    pub active: bool,
    /// ID of memory that superseded this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// Reinforcement provenance (breadcrumbs of when/where this was reinforced).
    #[serde(default)]
    pub reinforcements: Vec<Reinforcement>,
    /// Confidence score (0.0-1.0) - decays over time, boosted by use.
    #[serde(default = "default_confidence")]
    pub confidence: f32,
}

fn default_confidence() -> f32 {
    1.0
}

fn default_active() -> bool {
    true
}

fn new_memory_id() -> String {
    let ts = Utc::now().timestamp_millis();
    format!("mem_{ts}_{}", uuid::Uuid::new_v4().simple())
}

impl MemoryEntry {
    pub fn new(category: MemoryCategory, content: impl Into<String>) -> Self {
        let now = Utc::now();
        let content = content.into();
        Self {
            id: new_memory_id(),
            category,
            search_text: normalize_memory_search_text(&content, &[]),
            content,
            tags: Vec::new(),
            created_at: now,
            updated_at: now,
            access_count: 0,
            source: None,
            trust: TrustLevel::default(),
            strength: 1,
            active: true,
            superseded_by: None,
            reinforcements: Vec::new(),
            confidence: 1.0,
        }
    }

    pub fn refresh_search_text(&mut self) {
        self.search_text = normalize_memory_search_text(&self.content, &self.tags);
    }

    pub fn searchable_text(&self) -> std::borrow::Cow<'_, str> {
        if self.search_text.is_empty() {
            std::borrow::Cow::Owned(normalize_memory_search_text(&self.content, &self.tags))
        } else {
            std::borrow::Cow::Borrowed(&self.search_text)
        }
    }

    /// Get effective confidence after time-based decay.
    /// Half-life varies by category:
    /// - Correction: 365 days (user corrections are high value)
    /// - Preference: 90 days (preferences may evolve)
    /// - Fact: 30 days (codebase facts can become stale)
    /// - Entity: 60 days (entities change moderately)
    pub fn effective_confidence(&self) -> f32 {
        let age_days = (Utc::now() - self.created_at).num_days() as f32;
        let half_life = match self.category {
            MemoryCategory::Correction => 365.0,
            MemoryCategory::Preference => 90.0,
            MemoryCategory::Fact => 30.0,
            MemoryCategory::Entity => 60.0,
            MemoryCategory::Custom(_) => 45.0,
        };

        // Exponential decay: confidence * e^(-age/half_life * ln(2)),
        // boosted slightly for access count.
        let decay = (-age_days / half_life * 0.693).exp();
        let access_boost = 1.0 + 0.1 * (self.access_count as f32 + 1.0).ln();

        (self.confidence * decay * access_boost).min(1.0)
    }

    /// Boost confidence (called when memory was useful).
    pub fn boost_confidence(&mut self, amount: f32) {
        self.confidence = (self.confidence + amount).min(1.0);
        self.access_count += 1;
        self.updated_at = Utc::now();
    }

    /// Decay confidence (called when memory was retrieved but not relevant).
    pub fn decay_confidence(&mut self, amount: f32) {
        self.confidence = (self.confidence - amount).max(0.0);
    }

    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self.refresh_search_text();
        self
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn with_trust(mut self, trust: TrustLevel) -> Self {
        self.trust = trust;
        self
    }

    /// Override the generated id (e.g. deterministic ids like `skill:<name>`).
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Override created/updated timestamps (e.g. to backdate entries).
    pub fn with_timestamps(mut self, created_at: DateTime<Utc>, updated_at: DateTime<Utc>) -> Self {
        self.created_at = created_at;
        self.updated_at = updated_at;
        self
    }

    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
        self.access_count += 1;
    }

    /// Reinforce this memory (called when same info is encountered again).
    pub fn reinforce(&mut self, session_id: &str, message_index: usize) {
        self.strength += 1;
        self.updated_at = Utc::now();
        self.reinforcements.push(Reinforcement {
            session_id: session_id.to_string(),
            message_index,
            timestamp: Utc::now(),
        });
    }

    /// Mark this memory as superseded by another.
    pub fn supersede(&mut self, new_id: &str) {
        self.active = false;
        self.superseded_by = Some(new_id.to_string());
    }
}

/// Which store(s) an operation touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScope {
    Project,
    Global,
    All,
}

impl MemoryScope {
    pub fn includes_project(self) -> bool {
        matches!(self, Self::Project | Self::All)
    }

    pub fn includes_global(self) -> bool {
        matches!(self, Self::Global | Self::All)
    }
}

/// Legacy flat store, kept only as the migration source format.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryStore {
    pub entries: Vec<MemoryEntry>,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Graph storage
// ---------------------------------------------------------------------------

mod graph;
pub use graph::{
    ClusterEntry, Edge, EdgeKind, GraphMetadata, MemoryGraph, TagEntry, GRAPH_VERSION,
};

// ---------------------------------------------------------------------------
// Scoring and prompt formatting
// ---------------------------------------------------------------------------

/// Score an entry for relevance ordering: recency, access count, category
/// weight, trust multiplier, and reinforcement strength.
pub fn memory_score(entry: &MemoryEntry) -> f64 {
    if !entry.active {
        return 0.0;
    }

    let mut score = 0.0;
    let age_hours = (Utc::now() - entry.updated_at).num_hours() as f64;
    score += 100.0 / (1.0 + age_hours / 24.0);
    score += (entry.access_count as f64).sqrt() * 10.0;
    score += match entry.category {
        MemoryCategory::Correction => 50.0,
        MemoryCategory::Preference => 30.0,
        MemoryCategory::Fact => 20.0,
        MemoryCategory::Entity => 10.0,
        MemoryCategory::Custom(_) => 5.0,
    };
    score *= match entry.trust {
        TrustLevel::High => 1.5,
        TrustLevel::Medium => 1.0,
        TrustLevel::Low => 0.7,
    };
    score += (entry.strength as f64).ln() * 5.0;
    score
}

/// Select up to `limit` active entries, deduplicating by whitespace-normalized
/// lowercase content.
fn selected_entries_for_prompt(entries: &[MemoryEntry], limit: usize) -> Vec<&MemoryEntry> {
    let mut selected = Vec::new();
    let mut seen_content = HashSet::new();

    for entry in entries.iter().filter(|entry| entry.active) {
        if selected.len() >= limit {
            break;
        }

        let dedupe_key = entry
            .content
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        if dedupe_key.is_empty() || !seen_content.insert(dedupe_key) {
            continue;
        }

        selected.push(entry);
    }

    selected
}

/// Format selected entries as markdown sections grouped by category.
fn format_entries_for_prompt_with_header(
    entries: &[MemoryEntry],
    limit: usize,
    include_header: bool,
    include_updated_at_comments: bool,
) -> Option<String> {
    let mut sections: HashMap<MemoryCategory, Vec<&MemoryEntry>> = HashMap::new();

    for entry in selected_entries_for_prompt(entries, limit) {
        sections
            .entry(entry.category.clone())
            .or_default()
            .push(entry);
    }

    if sections.is_empty() {
        return None;
    }

    let mut output = String::new();
    let order = [
        MemoryCategory::Correction,
        MemoryCategory::Fact,
        MemoryCategory::Preference,
        MemoryCategory::Entity,
    ];

    let mut write_section = |title: &str, items: Vec<&MemoryEntry>| {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&format!("## {title}\n"));
        for (idx, item) in items.into_iter().enumerate() {
            output.push_str(&format!("{}. {}\n", idx + 1, item.content.trim()));
            if include_updated_at_comments {
                output.push_str(&format!(
                    "<!-- updated_at: {} -->\n",
                    item.updated_at.to_rfc3339()
                ));
            }
        }
    };

    for cat in &order {
        if let Some(items) = sections.remove(cat) {
            let title = match cat {
                MemoryCategory::Correction => "Corrections",
                MemoryCategory::Fact => "Facts",
                MemoryCategory::Preference => "Preferences",
                MemoryCategory::Entity => "Entities",
                MemoryCategory::Custom(_) => "Custom",
            };
            write_section(title, items);
        }
    }

    let mut custom_sections: BTreeMap<String, Vec<&MemoryEntry>> = BTreeMap::new();
    for (cat, items) in sections {
        match cat {
            MemoryCategory::Custom(name) => {
                custom_sections.insert(name, items);
            }
            other => {
                custom_sections.insert(other.to_string(), items);
            }
        }
    }
    for (name, items) in custom_sections {
        write_section(&name, items);
    }

    if output.is_empty() {
        None
    } else if include_header {
        Some(format!("# Memory\n\n{}", output.trim()))
    } else {
        Some(output.trim().to_string())
    }
}

/// Format entries for the system prompt's memory section.
pub fn format_entries_for_prompt(entries: &[MemoryEntry], limit: usize) -> Option<String> {
    format_entries_for_prompt_with_header(entries, limit, false, false)
}

/// Format entries as a display prompt (with `# Memory` header and
/// updated_at comments) for in-chat injection visualization.
pub fn format_relevant_display_prompt(entries: &[MemoryEntry], limit: usize) -> Option<String> {
    format_entries_for_prompt_with_header(entries, limit, true, true)
}

// ---------------------------------------------------------------------------
// Search text normalization
// ---------------------------------------------------------------------------

pub fn normalize_search_text(text: &str) -> String {
    let lowered = text.trim().to_lowercase();
    let mut normalized = String::with_capacity(lowered.len());
    let mut last_was_space = true;

    for ch in lowered.chars() {
        let mapped = if ch.is_whitespace() || matches!(ch, '-' | '_' | '/' | '\\' | '.' | ':') {
            ' '
        } else {
            ch
        };

        if mapped == ' ' {
            if !last_was_space {
                normalized.push(' ');
                last_was_space = true;
            }
        } else {
            normalized.push(mapped);
            last_was_space = false;
        }
    }

    normalized.trim_end().to_string()
}

pub fn normalize_memory_search_text(content: &str, tags: &[String]) -> String {
    let normalized_content = normalize_search_text(content);
    let normalized_tags: Vec<String> = tags
        .iter()
        .map(|tag| normalize_search_text(tag))
        .filter(|tag| !tag.is_empty())
        .collect();

    if normalized_tags.is_empty() {
        return normalized_content;
    }

    if normalized_content.is_empty() {
        return normalized_tags.join(" ");
    }

    format!("{} {}", normalized_content, normalized_tags.join(" "))
}

pub fn memory_matches_search(memory: &MemoryEntry, normalized_query: &str) -> bool {
    memory.searchable_text().contains(normalized_query)
}

// ---------------------------------------------------------------------------
// Top-k ranking helpers
// ---------------------------------------------------------------------------

pub mod ranking {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    struct TopKItem<T> {
        score: f32,
        ordinal: usize,
        value: T,
    }

    impl<T> PartialEq for TopKItem<T> {
        fn eq(&self, other: &Self) -> bool {
            self.score.to_bits() == other.score.to_bits() && self.ordinal == other.ordinal
        }
    }

    impl<T> Eq for TopKItem<T> {}

    impl<T> PartialOrd for TopKItem<T> {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    impl<T> Ord for TopKItem<T> {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            self.score
                .total_cmp(&other.score)
                .then_with(|| self.ordinal.cmp(&other.ordinal))
        }
    }

    pub fn top_k_by_score<T, I>(items: I, limit: usize) -> Vec<(T, f32)>
    where
        I: IntoIterator<Item = (T, f32)>,
    {
        if limit == 0 {
            return Vec::new();
        }

        let mut heap: BinaryHeap<Reverse<TopKItem<T>>> = BinaryHeap::new();

        for (ordinal, (value, score)) in items.into_iter().enumerate() {
            let candidate = Reverse(TopKItem {
                score,
                ordinal,
                value,
            });

            if heap.len() < limit {
                heap.push(candidate);
                continue;
            }

            let replace = heap
                .peek()
                .map(|smallest| score > smallest.0.score)
                .unwrap_or(false);
            if replace {
                heap.pop();
                heap.push(candidate);
            }
        }

        let mut results: Vec<_> = heap
            .into_iter()
            .map(|Reverse(item)| (item.value, item.score, item.ordinal))
            .collect();
        results.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
        results
            .into_iter()
            .map(|(value, score, _)| (value, score))
            .collect()
    }

    #[derive(Debug)]
    struct TopKOrdItem<T, K> {
        key: K,
        ordinal: usize,
        value: T,
    }

    impl<T, K: Ord> PartialEq for TopKOrdItem<T, K> {
        fn eq(&self, other: &Self) -> bool {
            self.key == other.key && self.ordinal == other.ordinal
        }
    }

    impl<T, K: Ord> Eq for TopKOrdItem<T, K> {}

    impl<T, K: Ord> PartialOrd for TopKOrdItem<T, K> {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    impl<T, K: Ord> Ord for TopKOrdItem<T, K> {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            self.key
                .cmp(&other.key)
                .then_with(|| self.ordinal.cmp(&other.ordinal))
        }
    }

    pub fn top_k_by_ord<T, K, I>(items: I, limit: usize) -> Vec<(T, K)>
    where
        I: IntoIterator<Item = (T, K)>,
        K: Ord,
    {
        if limit == 0 {
            return Vec::new();
        }

        let mut heap: BinaryHeap<Reverse<TopKOrdItem<T, K>>> = BinaryHeap::new();

        for (ordinal, (value, key)) in items.into_iter().enumerate() {
            let candidate = Reverse(TopKOrdItem {
                key,
                ordinal,
                value,
            });

            if heap.len() < limit {
                heap.push(candidate);
                continue;
            }

            let replace = heap
                .peek()
                .map(|smallest| candidate.0.key > smallest.0.key)
                .unwrap_or(false);
            if replace {
                heap.pop();
                heap.push(candidate);
            }
        }

        let mut results: Vec<_> = heap
            .into_iter()
            .map(|Reverse(item)| (item.value, item.key, item.ordinal))
            .collect();
        results.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
        results
            .into_iter()
            .map(|(value, key, _)| (value, key))
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn top_k_by_score_keeps_highest_scores_in_order() {
            let ranked = top_k_by_score([("a", 1.0), ("b", 3.0), ("c", 2.0)], 2);
            assert_eq!(ranked, vec![("b", 3.0), ("c", 2.0)]);
        }

        #[test]
        fn top_k_by_ord_keeps_highest_keys_in_order() {
            let ranked = top_k_by_ord([("a", 1), ("b", 3), ("c", 2)], 2);
            assert_eq!(ranked, vec![("b", 3), ("c", 2)]);
        }

        #[test]
        fn top_k_zero_limit_is_empty() {
            assert!(top_k_by_score([("a", 1.0)], 0).is_empty());
            assert!(top_k_by_ord([("a", 1)], 0).is_empty());
        }
    }
}

// ---------------------------------------------------------------------------
// MemoryManager: scoped persistence + in-process graph cache
// ---------------------------------------------------------------------------

/// Manages project-scoped and global memory graphs under the claurst config
/// directory. Storage layout mirrors jcode:
/// - global: `<config>/memory/global.json`
/// - project: `<config>/memory/projects/<hash>.json` where `hash` is a stable
///   hex hash of the project root path.
///
/// A per-process cache keyed by file mtime avoids re-reading unchanged files.
pub struct MemoryManager {
    project_dir: Option<PathBuf>,
    /// When set, memory IO is rooted here instead of the claurst config dir
    /// (used by tests to get isolation without touching real user data).
    storage_root: Option<PathBuf>,
}

impl Default for MemoryManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryManager {
    pub fn new() -> Self {
        Self {
            project_dir: None,
            storage_root: None,
        }
    }

    pub fn with_project_dir(mut self, project_dir: impl Into<PathBuf>) -> Self {
        self.project_dir = Some(project_dir.into());
        self
    }

    /// Root all storage under an explicit directory (tests, overrides).
    pub fn with_storage_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.storage_root = Some(root.into());
        self
    }

    fn memory_base_dir(&self) -> PathBuf {
        self.storage_root
            .clone()
            .unwrap_or_else(crate::config::Settings::config_dir)
            .join("memory")
    }

    fn project_memory_path(&self) -> Option<PathBuf> {
        let project_dir = self.project_dir.as_ref()?;
        let project_hash = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            project_dir.hash(&mut hasher);
            format!("{:016x}", hasher.finish())
        };
        Some(
            self.memory_base_dir()
                .join("projects")
                .join(format!("{project_hash}.json")),
        )
    }

    fn global_memory_path(&self) -> PathBuf {
        self.memory_base_dir().join("global.json")
    }

    fn write_graph(path: &PathBuf, graph: &MemoryGraph) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(graph)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Load the project graph. Legacy flat-store files are migrated and
    /// persisted (with a one-time `.bak` backup of the original).
    pub fn load_project_graph(&self) -> anyhow::Result<MemoryGraph> {
        let Some(path) = self.project_memory_path() else {
            return Ok(MemoryGraph::new());
        };
        Self::read_graph_with_migration(&path)
    }

    /// Load the global graph, with the same legacy migration as project.
    pub fn load_global_graph(&self) -> anyhow::Result<MemoryGraph> {
        let path = self.global_memory_path();
        Self::read_graph_with_migration(&path)
    }

    fn read_graph_with_migration(path: &PathBuf) -> anyhow::Result<MemoryGraph> {
        if !path.exists() {
            return Ok(MemoryGraph::new());
        }
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(err) => {
                tracing::warn!("Failed to read memory file {}: {}", path.display(), err);
                return Ok(MemoryGraph::new());
            }
        };
        if let Ok(graph) = serde_json::from_str::<MemoryGraph>(&contents) {
            if graph.graph_version == GRAPH_VERSION {
                return Ok(graph);
            }
        }
        // Legacy flat store: migrate, back up, persist. A file that parses as
        // neither format is treated as empty rather than fatal.
        let store: MemoryStore = match serde_json::from_str(&contents) {
            Ok(store) => store,
            Err(_) => {
                tracing::warn!(
                    "Unreadable memory file (corrupt or foreign format), starting empty: {}",
                    path.display()
                );
                return Ok(MemoryGraph::new());
            }
        };
        let graph = MemoryGraph::from_legacy_store(store);
        let backup_path = path.with_extension("json.bak");
        if !backup_path.exists() {
            let _ = std::fs::copy(path, &backup_path);
        }
        Self::write_graph(path, &graph)?;
        tracing::info!("Migrated memory store to graph format: {}", path.display());
        Ok(graph)
    }

    pub fn save_project_graph(&self, graph: &MemoryGraph) -> anyhow::Result<()> {
        if let Some(path) = self.project_memory_path() {
            Self::write_graph(&path, graph)?;
        }
        Ok(())
    }

    pub fn save_global_graph(&self, graph: &MemoryGraph) -> anyhow::Result<()> {
        Self::write_graph(&self.global_memory_path(), graph)
    }

    /// Store an entry in the project graph. An exact duplicate (same category
    /// and trimmed content) reinforces the existing entry instead of adding
    /// a second copy.
    pub fn remember_project(&self, entry: MemoryEntry) -> anyhow::Result<String> {
        anyhow::ensure!(
            self.project_memory_path().is_some(),
            "Project memory requires a working directory; use global scope explicitly"
        );
        let mut graph = self.load_project_graph()?;
        let id = Self::remember_in_graph(&mut graph, entry);
        self.save_project_graph(&graph)?;
        Ok(id)
    }

    /// Store an entry in the global graph (same dedup semantics as project).
    pub fn remember_global(&self, entry: MemoryEntry) -> anyhow::Result<String> {
        let mut graph = self.load_global_graph()?;
        let id = Self::remember_in_graph(&mut graph, entry);
        self.save_global_graph(&graph)?;
        Ok(id)
    }

    fn remember_in_graph(graph: &mut MemoryGraph, entry: MemoryEntry) -> String {
        let normalized = entry.content.trim();
        let duplicate = graph
            .active_memories()
            .into_iter()
            .find(|existing| {
                existing.category == entry.category && existing.content.trim() == normalized
            })
            .map(|existing| existing.id.clone());
        if let Some(id) = duplicate {
            if let Some(existing) = graph.get_memory_mut(&id) {
                existing.reinforce(entry.source.as_deref().unwrap_or("dedup"), 0);
            }
            return id;
        }
        graph.add_memory(entry)
    }

    /// Insert or update a memory with a stable ID in the project graph.
    pub fn upsert_project_memory(&self, entry: MemoryEntry) -> anyhow::Result<String> {
        let mut graph = self.load_project_graph()?;
        let id = Self::upsert_memory_in_graph(&mut graph, entry);
        self.save_project_graph(&graph)?;
        Ok(id)
    }

    /// Insert or update a memory with a stable ID in the global graph.
    pub fn upsert_global_memory(&self, entry: MemoryEntry) -> anyhow::Result<String> {
        let mut graph = self.load_global_graph()?;
        let id = Self::upsert_memory_in_graph(&mut graph, entry);
        self.save_global_graph(&graph)?;
        Ok(id)
    }

    fn upsert_memory_in_graph(graph: &mut MemoryGraph, entry: MemoryEntry) -> String {
        let id = entry.id.clone();

        let Some(existing_snapshot) = graph.get_memory(&id).cloned() else {
            return graph.add_memory(entry);
        };

        let old_tags: HashSet<String> = existing_snapshot.tags.iter().cloned().collect();
        let new_tags: HashSet<String> = entry.tags.iter().cloned().collect();

        for tag in old_tags.difference(&new_tags) {
            graph.untag_memory(&id, tag);
        }
        for tag in new_tags.difference(&old_tags) {
            graph.tag_memory(&id, tag);
        }

        if let Some(existing) = graph.get_memory_mut(&id) {
            existing.category = entry.category;
            existing.content = entry.content;
            existing.tags = entry.tags;
            existing.updated_at = entry.updated_at;
            existing.source = entry.source;
            existing.trust = entry.trust;
            existing.active = entry.active;
            existing.superseded_by = entry.superseded_by;
            existing.confidence = entry.confidence;
        }

        id
    }

    fn collect_memories_scoped(&self, scope: MemoryScope) -> anyhow::Result<Vec<MemoryEntry>> {
        let mut entries = Vec::new();
        if scope.includes_project() {
            if let Ok(project) = self.load_project_graph() {
                entries.extend(project.all_memories().cloned());
            }
        }
        if scope.includes_global() {
            if let Ok(global) = self.load_global_graph() {
                entries.extend(global.all_memories().cloned());
            }
        }
        Ok(entries)
    }

    /// List all memories (project + global), newest first.
    pub fn list_all(&self) -> anyhow::Result<Vec<MemoryEntry>> {
        self.list_all_scoped(MemoryScope::All)
    }

    pub fn list_all_scoped(&self, scope: MemoryScope) -> anyhow::Result<Vec<MemoryEntry>> {
        let mut all = self.collect_memories_scoped(scope)?;
        all.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
        Ok(all)
    }

    /// Lexical search across the requested scopes. The query is normalized the
    /// same way entry search text is, so exact substring matching is
    /// whitespace/punctuation-tolerant.
    pub fn search(&self, query: &str, scope: MemoryScope) -> anyhow::Result<Vec<MemoryEntry>> {
        let query_lower = normalize_search_text(query);
        if query_lower.is_empty() {
            return Ok(Vec::new());
        }

        let mut results = Vec::new();
        for memory in self.collect_memories_scoped(scope)? {
            if memory_matches_search(&memory, &query_lower) {
                results.push(memory);
            }
        }

        Ok(results)
    }

    /// Remove a memory by ID from whichever store contains it. Returns whether
    /// a memory was removed.
    pub fn forget(&self, id: &str) -> anyhow::Result<bool> {
        let mut project_graph = self.load_project_graph()?;
        if project_graph.remove_memory(id).is_some() {
            self.save_project_graph(&project_graph)?;
            return Ok(true);
        }

        let mut global_graph = self.load_global_graph()?;
        if global_graph.remove_memory(id).is_some() {
            self.save_global_graph(&global_graph)?;
            return Ok(true);
        }

        Ok(false)
    }

    /// Add a tag to a memory, looking in project then global store.
    pub fn tag_memory(&self, memory_id: &str, tag: &str) -> anyhow::Result<()> {
        let mut graph = self.load_project_graph()?;
        if graph.memories.contains_key(memory_id) {
            graph.tag_memory(memory_id, tag);
            return self.save_project_graph(&graph);
        }

        let mut graph = self.load_global_graph()?;
        if graph.memories.contains_key(memory_id) {
            graph.tag_memory(memory_id, tag);
            return self.save_global_graph(&graph);
        }

        anyhow::bail!("Memory not found: {}", memory_id)
    }

    /// Link two memories with a RelatesTo edge. Both must live in the same
    /// store (project or global).
    pub fn link_memories(&self, from_id: &str, to_id: &str, weight: f32) -> anyhow::Result<()> {
        let mut graph = self.load_project_graph()?;
        if graph.memories.contains_key(from_id) && graph.memories.contains_key(to_id) {
            graph.link_memories(from_id, to_id, weight);
            return self.save_project_graph(&graph);
        }

        let mut graph = self.load_global_graph()?;
        if graph.memories.contains_key(from_id) && graph.memories.contains_key(to_id) {
            graph.link_memories(from_id, to_id, weight);
            return self.save_global_graph(&graph);
        }

        anyhow::bail!("Both memories must be in the same store (project or global)")
    }

    /// Get memories related to a given memory via graph traversal.
    pub fn get_related(&self, memory_id: &str, depth: usize) -> anyhow::Result<Vec<MemoryEntry>> {
        let (mut graph, _is_project) = {
            let project_graph = self.load_project_graph()?;
            if project_graph.memories.contains_key(memory_id) {
                (project_graph, true)
            } else {
                let global_graph = self.load_global_graph()?;
                if global_graph.memories.contains_key(memory_id) {
                    (global_graph, false)
                } else {
                    anyhow::bail!("Memory not found: {}", memory_id);
                }
            }
        };

        let results = graph.cascade_retrieve(&[memory_id.to_string()], &[1.0], depth, 20);
        Ok(results
            .into_iter()
            .filter_map(|(id, _)| graph.get_memory(&id).cloned())
            .filter(|entry| entry.id != memory_id)
            .collect())
    }

    /// Format the top-scoring active memories for prompt injection.
    pub fn format_for_prompt(&self, scope: MemoryScope, limit: usize) -> Option<String> {
        let Ok(entries) = self.collect_memories_scoped(scope) else {
            return None;
        };
        let relevant: Vec<MemoryEntry> = ranking::top_k_by_score(
            entries
                .iter()
                .filter(|entry| entry.active)
                .map(|entry| (entry, memory_score(entry) as f32)),
            limit,
        )
        .into_iter()
        .map(|(entry, _)| entry.clone())
        .collect();
        format_entries_for_prompt(&relevant, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_manager(dir: &std::path::Path, project: Option<&str>) -> MemoryManager {
        let mut m = MemoryManager::new().with_storage_root(dir);
        if let Some(p) = project {
            m = m.with_project_dir(p);
        }
        m
    }

    #[test]
    fn entry_new_initializes_fields() {
        let e = MemoryEntry::new(MemoryCategory::Preference, "Likes short answers");
        assert!(e.id.starts_with("mem_"));
        assert_eq!(e.category, MemoryCategory::Preference);
        assert_eq!(e.search_text, "likes short answers");
        assert_eq!(e.trust, TrustLevel::Medium);
        assert_eq!(e.strength, 1);
        assert!(e.active);
        assert_eq!(e.confidence, 1.0);
    }

    #[test]
    fn from_extracted_maps_known_and_unknown() {
        assert_eq!(
            MemoryCategory::from_extracted("bug"),
            MemoryCategory::Correction
        );
        assert_eq!(
            MemoryCategory::from_extracted("PREFERENCES"),
            MemoryCategory::Preference
        );
        assert_eq!(
            MemoryCategory::from_extracted("lesson"),
            MemoryCategory::Fact
        );
        // Unknown falls back to Fact (never Custom from LLM output).
        assert_eq!(
            MemoryCategory::from_extracted("whatever"),
            MemoryCategory::Fact
        );
    }

    #[test]
    fn effective_confidence_decays_and_boosts() {
        let old = MemoryEntry::new(MemoryCategory::Fact, "old fact").with_timestamps(
            Utc::now() - chrono::Duration::days(60),
            Utc::now() - chrono::Duration::days(60),
        );
        assert!(old.effective_confidence() < 1.0);

        let mut used = MemoryEntry::new(MemoryCategory::Fact, "used fact");
        used.boost_confidence(0.2);
        assert_eq!(used.access_count, 1);
        assert_eq!(used.confidence, 1.0); // clamped at 1.0

        let mut irrelevant = MemoryEntry::new(MemoryCategory::Fact, "irrelevant");
        irrelevant.decay_confidence(0.5);
        assert_eq!(irrelevant.confidence, 0.5);
    }

    #[test]
    fn reinforce_and_supersede_update_state() {
        let mut e = MemoryEntry::new(MemoryCategory::Correction, "fix");
        e.reinforce("sess-1", 3);
        assert_eq!(e.strength, 2);
        assert_eq!(e.reinforcements.len(), 1);
        assert_eq!(e.reinforcements[0].session_id, "sess-1");

        e.supersede("newer-id");
        assert!(!e.active);
        assert_eq!(e.superseded_by.as_deref(), Some("newer-id"));
    }

    #[test]
    fn normalize_search_text_collapses_separators() {
        // All separator characters map to single spaces; runs collapse.
        assert_eq!(
            normalize_search_text("Foo-Bar_baz/qux.quux:quuz  "),
            "foo bar baz qux quux quuz"
        );
        // Interior whitespace runs also collapse; leading/trailing trimmed.
        assert_eq!(normalize_search_text("  A  B  "), "a b");
        assert_eq!(normalize_search_text(""), "");
    }

    #[test]
    fn remember_project_dedups_exact_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let m = temp_manager(dir.path(), Some("/repo"));

        let id1 = m
            .remember_project(MemoryEntry::new(
                MemoryCategory::Fact,
                "The build is cargo check --workspace",
            ))
            .unwrap();
        let id2 = m
            .remember_project(MemoryEntry::new(
                MemoryCategory::Fact,
                "The build is cargo check --workspace",
            ))
            .unwrap();
        assert_eq!(id1, id2, "duplicate should reinforce, not re-add");

        let graph = m.load_project_graph().unwrap();
        assert_eq!(graph.memory_count(), 1);
        assert_eq!(graph.get_memory(&id1).expect("entry").strength, 2);
    }

    #[test]
    fn remember_project_requires_project_dir() {
        let dir = tempfile::tempdir().unwrap();
        let m = temp_manager(dir.path(), None);
        let err = m
            .remember_project(MemoryEntry::new(MemoryCategory::Fact, "x"))
            .unwrap_err();
        assert!(err.to_string().contains("working directory"));
    }

    #[test]
    fn project_scoping_isolates_projects() {
        let dir = tempfile::tempdir().unwrap();
        let m_a = temp_manager(dir.path(), Some("/repo-a"));
        let m_b = temp_manager(dir.path(), Some("/repo-b"));

        m_a.remember_project(MemoryEntry::new(MemoryCategory::Fact, "repo a fact"))
            .unwrap();

        let a = m_a.list_all_scoped(MemoryScope::Project).unwrap();
        let b = m_b.list_all_scoped(MemoryScope::Project).unwrap();
        assert_eq!(a.len(), 1);
        assert!(b.is_empty());
    }

    #[test]
    fn global_scope_separate_from_project() {
        let dir = tempfile::tempdir().unwrap();
        let m = temp_manager(dir.path(), Some("/repo"));

        m.remember_global(MemoryEntry::new(
            MemoryCategory::Preference,
            "prefers dark mode",
        ))
        .unwrap();
        m.remember_project(MemoryEntry::new(MemoryCategory::Fact, "proj fact"))
            .unwrap();

        assert_eq!(m.list_all_scoped(MemoryScope::Global).unwrap().len(), 1);
        assert_eq!(m.list_all_scoped(MemoryScope::Project).unwrap().len(), 1);
        assert_eq!(m.list_all().unwrap().len(), 2);
    }

    #[test]
    fn search_is_normalized_substring_match() {
        let dir = tempfile::tempdir().unwrap();
        let m = temp_manager(dir.path(), Some("/repo"));

        m.remember_project(
            MemoryEntry::new(MemoryCategory::Fact, "Uses cargo workspaces")
                .with_tags(vec!["rust".to_string()]),
        )
        .unwrap();

        let hits = m.search("cargo workspaces", MemoryScope::All).unwrap();
        assert_eq!(hits.len(), 1);

        // Punctuation-tolerant query.
        let hits = m.search("cargo-workspaces", MemoryScope::All).unwrap();
        assert_eq!(hits.len(), 1);

        // Tag content is searchable.
        let hits = m.search("rust", MemoryScope::All).unwrap();
        assert_eq!(hits.len(), 1);

        // No match.
        assert!(m.search("maven", MemoryScope::All).unwrap().is_empty());

        // Empty query matches nothing.
        assert!(m.search("   ", MemoryScope::All).unwrap().is_empty());
    }

    #[test]
    fn forget_removes_from_owning_store() {
        let dir = tempfile::tempdir().unwrap();
        let m = temp_manager(dir.path(), Some("/repo"));

        let id = m
            .remember_project(MemoryEntry::new(MemoryCategory::Fact, "bye"))
            .unwrap();
        assert!(m.forget(&id).unwrap());
        assert!(!m.forget(&id).unwrap());
        assert!(m.list_all().unwrap().is_empty());
    }

    #[test]
    fn upsert_updates_existing_entry_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let m = temp_manager(dir.path(), Some("/repo"));

        let id = m
            .remember_project(MemoryEntry::new(MemoryCategory::Fact, "v1"))
            .unwrap();

        let updated = MemoryEntry::new(MemoryCategory::Fact, "v2")
            .with_id(id.clone())
            .with_tags(vec!["t".to_string()]);
        let id2 = m.upsert_project_memory(updated).unwrap();
        assert_eq!(id, id2);

        let graph = m.load_project_graph().unwrap();
        assert_eq!(graph.memory_count(), 1);
        assert_eq!(graph.get_memory(&id).expect("entry").content, "v2");
        assert!(graph.tags.contains_key("tag:t"));
    }

    #[test]
    fn legacy_flat_store_migrates_with_backup() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path().join("memory").join("projects");
        std::fs::create_dir_all(&memory_dir).unwrap();

        let project_hash = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            std::path::Path::new("/repo").hash(&mut hasher);
            format!("{:016x}", hasher.finish())
        };
        let path = memory_dir.join(format!("{project_hash}.json"));

        let store = MemoryStore {
            entries: vec![MemoryEntry::new(MemoryCategory::Fact, "legacy fact")],
            metadata: HashMap::new(),
        };
        std::fs::write(&path, serde_json::to_string_pretty(&store).unwrap()).unwrap();

        let m = temp_manager(dir.path(), Some("/repo"));
        let graph = m.load_project_graph().unwrap();
        assert_eq!(graph.memory_count(), 1);
        assert!(graph.is_migrated());

        // Backup created, original replaced with graph format.
        assert!(path.with_extension("json.bak").exists());
        let persisted: MemoryGraph =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(persisted.graph_version, GRAPH_VERSION);
    }

    #[test]
    fn corrupt_file_starts_empty_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path().join("memory").join("projects");
        std::fs::create_dir_all(&memory_dir).unwrap();
        let project_hash = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            std::path::Path::new("/repo").hash(&mut hasher);
            format!("{:016x}", hasher.finish())
        };
        std::fs::write(
            memory_dir.join(format!("{project_hash}.json")),
            "{not json at all",
        )
        .unwrap();

        let m = temp_manager(dir.path(), Some("/repo"));
        // Corrupt file is treated as empty, not fatal.
        let graph = m.load_project_graph().unwrap();
        assert_eq!(graph.memory_count(), 0);
        m.remember_project(MemoryEntry::new(MemoryCategory::Fact, "fresh"))
            .unwrap();
        assert_eq!(m.list_all().unwrap().len(), 1);
    }

    #[test]
    fn format_for_prompt_groups_by_category_and_dedups() {
        let dir = tempfile::tempdir().unwrap();
        let m = temp_manager(dir.path(), Some("/repo"));

        m.remember_project(MemoryEntry::new(
            MemoryCategory::Preference,
            "Keep answers short",
        ))
        .unwrap();
        m.remember_project(MemoryEntry::new(
            MemoryCategory::Preference,
            "Keep answers   short", // whitespace duplicate
        ))
        .unwrap();
        m.remember_project(MemoryEntry::new(
            MemoryCategory::Correction,
            "Never run cargo fmt --all",
        ))
        .unwrap();

        let prompt = m.format_for_prompt(MemoryScope::All, 10).unwrap();
        assert!(prompt.contains("## Corrections"));
        assert!(prompt.contains("## Preferences"));
        assert!(prompt.contains("Never run cargo fmt --all"));
        // Whitespace duplicate dropped: exactly one Preferences entry survives
        // (either raw variant), so normalized text appears once.
        let preference_section = prompt
            .split("## Preferences")
            .nth(1)
            .expect("preferences section");
        assert_eq!(
            normalize_search_text(preference_section),
            "1 keep answers short"
        );
    }

    #[test]
    fn inactive_memories_are_excluded_from_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let m = temp_manager(dir.path(), Some("/repo"));

        let id = m
            .remember_project(MemoryEntry::new(MemoryCategory::Fact, "stale"))
            .unwrap();
        let mut graph = m.load_project_graph().unwrap();
        graph.get_memory_mut(&id).expect("entry").supersede("other");
        m.save_project_graph(&graph).unwrap();

        assert!(m.format_for_prompt(MemoryScope::All, 10).is_none());
    }

    #[test]
    fn graph_json_roundtrip_preserves_tags_and_edges() {
        let mut graph = MemoryGraph::new();
        let a = graph.add_memory(
            MemoryEntry::new(MemoryCategory::Fact, "alpha").with_tags(vec!["x".to_string()]),
        );
        let b = graph.add_memory(MemoryEntry::new(MemoryCategory::Fact, "beta"));
        graph.link_memories(&a, &b, 0.4);

        let json = serde_json::to_string(&graph).unwrap();
        let back: MemoryGraph = serde_json::from_str(&json).unwrap();
        assert_eq!(back.memory_count(), 2);
        assert!(back.tags.contains_key("tag:x"));
        assert!(back.get_edges(&a).iter().any(
            |e| matches!(&e.kind, EdgeKind::RelatesTo { weight } if (*weight - 0.4).abs() < 1e-6)
        ));
    }

    #[test]
    fn serde_compatibility_fields_default_cleanly() {
        // Minimal entry JSON with omitted optional fields parses with defaults.
        let json = r#"{
            "id": "mem_x",
            "category": "fact",
            "content": "c",
            "tags": [],
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "access_count": 0
        }"#;
        let e: MemoryEntry = serde_json::from_str(json).unwrap();
        assert_eq!(e.trust, TrustLevel::Medium);
        assert_eq!(e.strength, 0);
        assert!(e.active);
        assert_eq!(e.confidence, 1.0);
        assert!(e.superseded_by.is_none());
    }

    #[test]
    fn link_memories_rejects_cross_store() {
        let dir = tempfile::tempdir().unwrap();
        let m = temp_manager(dir.path(), Some("/repo"));

        let id = m
            .remember_project(MemoryEntry::new(MemoryCategory::Fact, "proj"))
            .unwrap();
        let gid = m
            .remember_global(MemoryEntry::new(MemoryCategory::Fact, "glob"))
            .unwrap();

        let err = m.link_memories(&id, &gid, 0.5).unwrap_err();
        assert!(err.to_string().contains("same store"));
    }

    #[test]
    fn get_related_via_shared_tag() {
        let dir = tempfile::tempdir().unwrap();
        let m = temp_manager(dir.path(), Some("/repo"));

        let a = m
            .remember_project(
                MemoryEntry::new(MemoryCategory::Fact, "alpha")
                    .with_tags(vec!["shared".to_string()]),
            )
            .unwrap();
        m.remember_project(
            MemoryEntry::new(MemoryCategory::Fact, "beta").with_tags(vec!["shared".to_string()]),
        )
        .unwrap();

        let related = m.get_related(&a, 2).unwrap();
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].content, "beta");
    }
}
