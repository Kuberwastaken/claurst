// GrowthBook feature flags integration
//
// Provides a feature flag manager that fetches flags from GrowthBook API,
// caches them locally, and provides a simple API for checking flag values.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::fs;
use tracing::{debug, warn};

/// Represents a feature flag from GrowthBook
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureFlag {
    /// The ID of the feature flag
    pub id: String,
    /// The key of the feature flag (used for lookups)
    pub key: String,
    /// Whether the feature is enabled
    pub enabled: bool,
    /// Optional: the variant (A/B test group, etc.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// Cached feature flags with metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedFlags {
    /// Map of flag key to flag
    flags: HashMap<String, FeatureFlag>,
    /// When the cache was fetched (Unix timestamp)
    fetched_at: u64,
}

/// Manages feature flags from GrowthBook
#[derive(Clone)]
pub struct FeatureFlagManager {
    /// Map of flag key to flag value
    flags: Arc<parking_lot::RwLock<HashMap<String, bool>>>,
    /// Cache file path
    cache_path: PathBuf,
    /// GrowthBook API endpoint
    api_endpoint: String,
    /// Cache TTL in seconds (default: 1 hour)
    cache_ttl: u64,
    /// HTTP client for making requests
    http_client: reqwest::Client,
}

impl Default for FeatureFlagManager {
    fn default() -> Self {
        Self::new()
    }
}

impl FeatureFlagManager {
    /// Create a new feature flag manager
    ///
    /// The API key is automatically fetched from the GROWTHBOOK_API_KEY environment variable.
    pub fn new() -> Self {
        let cache_path = Self::get_cache_path();
        let api_endpoint = "https://api.growthbook.io/api/features".to_string();
        let cache_ttl = 3600; // 1 hour

        Self {
            flags: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            cache_path,
            api_endpoint,
            cache_ttl,
            http_client: reqwest::Client::new(),
        }
    }

    /// Get the cache file path (~/.claurst/feature_flags.json)
    fn get_cache_path() -> PathBuf {
        crate::config::Settings::config_dir().join("feature_flags.json")
    }

    /// Check if a feature flag is enabled
    ///
    /// # Arguments
    /// * `name` - The feature flag key to check
    pub fn flag(&self, name: &str) -> bool {
        let flags = self.flags.read();
        flags.get(name).copied().unwrap_or(false)
    }

    /// Fetch flags from GrowthBook API
    ///
    /// This is an async operation that:
    /// 1. Checks if cached flags are still valid (within TTL)
    /// 2. If cache is stale, fetches from GrowthBook API
    /// 3. Saves the response to the cache file
    /// 4. Updates the in-memory flags
    pub async fn fetch_flags_async(&self) -> Result<()> {
        // Try to load from cache first
        if let Ok(cached) = self.load_cached_flags().await {
            if self.is_cache_valid(&cached) {
                debug!("Using cached feature flags");
                self.update_flags_from_cached(&cached);
                return Ok(());
            }
        }

        // Cache is stale or missing, fetch from API
        debug!("Fetching feature flags from GrowthBook API");
        match self.fetch_from_api().await {
            Ok(cached) => {
                // Save to cache
                if let Err(e) = self.save_cached_flags(&cached).await {
                    warn!("Failed to save feature flags cache: {}", e);
                    // Don't fail the whole operation if we can't save cache
                }
                self.update_flags_from_cached(&cached);
                Ok(())
            }
            Err(e) => {
                // If API fetch fails, try to use stale cache
                warn!("Failed to fetch from GrowthBook API: {}", e);
                if let Ok(cached) = self.load_cached_flags().await {
                    debug!("Using stale cached feature flags as fallback");
                    self.update_flags_from_cached(&cached);
                    return Ok(());
                }
                // No cache available, just warn and continue with defaults
                warn!("No cached feature flags available, using defaults");
                Ok(())
            }
        }
    }

    /// Fetch flags from GrowthBook API
    async fn fetch_from_api(&self) -> Result<CachedFlags> {
        let api_key = std::env::var("GROWTHBOOK_API_KEY").ok();

        let mut builder = self.http_client.get(&self.api_endpoint);

        // Add authorization header if API key is available
        if let Some(key) = api_key {
            builder = builder.header("Authorization", format!("Bearer {}", key));
        }

        let response = builder
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .context("Failed to fetch from GrowthBook API")?;

        let status = response.status();
        if !status.is_success() {
            return Err(anyhow!(
                "GrowthBook API returned status {}: {}",
                status.as_u16(),
                response.text().await.unwrap_or_default()
            ));
        }

        let body = response
            .json::<GrowthBookApiResponse>()
            .await
            .context("Failed to parse GrowthBook API response")?;

        Ok(CachedFlags {
            flags: body
                .features
                .into_iter()
                .map(|f| (f.key.clone(), f))
                .collect(),
            fetched_at: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        })
    }

    /// Check if cached flags are still valid (within TTL)
    fn is_cache_valid(&self, cached: &CachedFlags) -> bool {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now - cached.fetched_at < self.cache_ttl
    }

    /// Load cached flags from disk
    async fn load_cached_flags(&self) -> Result<CachedFlags> {
        let data = fs::read_to_string(&self.cache_path)
            .await
            .context("Failed to read cache file")?;
        let cached: CachedFlags =
            serde_json::from_str(&data).context("Failed to parse cache file")?;
        Ok(cached)
    }

    /// Save cached flags to disk
    async fn save_cached_flags(&self, cached: &CachedFlags) -> Result<()> {
        // Ensure the directory exists
        if let Some(parent) = self.cache_path.parent() {
            fs::create_dir_all(parent).await.ok();
        }

        let json = serde_json::to_string(cached).context("Failed to serialize cache")?;
        fs::write(&self.cache_path, json)
            .await
            .context("Failed to write cache file")?;
        Ok(())
    }

    /// Update in-memory flags from cached data
    fn update_flags_from_cached(&self, cached: &CachedFlags) {
        let mut flags = self.flags.write();
        flags.clear();
        for (key, flag) in &cached.flags {
            flags.insert(key.clone(), flag.enabled);
        }
        debug!("Loaded {} feature flags", flags.len());
    }
}

/// Response from GrowthBook API
#[derive(Debug, Deserialize)]
struct GrowthBookApiResponse {
    /// Map of feature flag keys to flag objects
    pub features: Vec<FeatureFlag>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_path() {
        let path = FeatureFlagManager::get_cache_path();
        // "claurst" (not ".claurst"): the config dir is legacy ~/.claurst on
        // existing installs but XDG ~/.config/claurst on fresh ones (#207).
        assert!(path.to_string_lossy().contains("claurst"));
        assert!(path.to_string_lossy().contains("feature_flags.json"));
    }

    #[test]
    fn test_flag_default_false() {
        let manager = FeatureFlagManager::new();
        assert!(!manager.flag("nonexistent_flag"));
    }

    #[test]
    fn test_cache_validity() {
        let manager = FeatureFlagManager::new();
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Fresh cache
        let fresh_cache = CachedFlags {
            flags: HashMap::new(),
            fetched_at: now,
        };
        assert!(manager.is_cache_valid(&fresh_cache));

        // Stale cache (older than TTL)
        let stale_cache = CachedFlags {
            flags: HashMap::new(),
            fetched_at: now - 7200, // 2 hours ago
        };
        assert!(!manager.is_cache_valid(&stale_cache));
    }
}
