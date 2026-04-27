//! In-memory response cache for frequently requested routes.
//!
//! Uses a simple TTL-based strategy: cached entries are served until they
//! expire, at which point the next request re-fetches and repopulates the
//! cache.  This avoids thundering-herd issues on the `/metrics` endpoint
//! which is scraped by Prometheus at a fixed interval.
//!
//! ## Cache invalidation
//!
//! Entries expire after a configurable TTL (default: 10 seconds).  The TTL
//! is intentionally shorter than the Prometheus scrape interval so that
//! metrics are always fresh within one scrape cycle.
//!
//! ## Configuration
//!
//! | Env var                    | Default | Description                        |
//! |----------------------------|---------|------------------------------------|
//! | `ROUTER_CACHE_TTL_SECS`    | `10`    | Entry TTL in seconds               |
//! | `ROUTER_CACHE_ENABLED`     | `true`  | Set to `false` to disable caching  |

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use dashmap::DashMap;

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub ttl: Duration,
    pub enabled: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(10),
            enabled: true,
        }
    }
}

pub fn config_from_env() -> CacheConfig {
    let ttl_secs = std::env::var("ROUTER_CACHE_TTL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10u64);

    let enabled = std::env::var("ROUTER_CACHE_ENABLED")
        .map(|v| v != "false" && v != "0")
        .unwrap_or(true);

    CacheConfig {
        ttl: Duration::from_secs(ttl_secs),
        enabled,
    }
}

// ── Cache entry ───────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Entry {
    body: Vec<u8>,
    content_type: String,
    inserted_at: Instant,
}

// ── Cache ─────────────────────────────────────────────────────────────────────

/// Thread-safe in-memory response cache.  Clone is cheap (Arc inside).
#[derive(Clone)]
pub struct ResponseCache {
    config: CacheConfig,
    store: Arc<DashMap<String, Entry>>,
}

impl ResponseCache {
    pub fn new(config: CacheConfig) -> Self {
        Self {
            config,
            store: Arc::new(DashMap::new()),
        }
    }

    /// Try to retrieve a cached response for `key`.
    ///
    /// Returns `None` if caching is disabled, the entry is missing, or the
    /// entry has expired (and removes the stale entry).
    pub fn get(&self, key: &str) -> Option<(Vec<u8>, String)> {
        if !self.config.enabled {
            return None;
        }
        if let Some(entry) = self.store.get(key) {
            if entry.inserted_at.elapsed() < self.config.ttl {
                return Some((entry.body.clone(), entry.content_type.clone()));
            }
        }
        // Expired — remove it
        self.store.remove(key);
        None
    }

    /// Insert or replace a cache entry for `key`.
    pub fn set(&self, key: &str, body: Vec<u8>, content_type: impl Into<String>) {
        if !self.config.enabled {
            return;
        }
        self.store.insert(
            key.to_string(),
            Entry {
                body,
                content_type: content_type.into(),
                inserted_at: Instant::now(),
            },
        );
    }

    /// Remove all expired entries.  Call periodically to avoid unbounded growth.
    pub fn evict_expired(&self) {
        let ttl = self.config.ttl;
        self.store.retain(|_, v| v.inserted_at.elapsed() < ttl);
    }

    /// Return the number of live (non-expired) entries.
    pub fn len(&self) -> usize {
        self.store
            .iter()
            .filter(|e| e.inserted_at.elapsed() < self.config.ttl)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(ttl_secs: u64) -> ResponseCache {
        ResponseCache::new(CacheConfig {
            ttl: Duration::from_secs(ttl_secs),
            enabled: true,
        })
    }

    #[test]
    fn miss_on_empty_cache() {
        let c = cache(10);
        assert!(c.get("metrics").is_none());
    }

    #[test]
    fn hit_after_set() {
        let c = cache(10);
        c.set("metrics", b"data".to_vec(), "text/plain");
        let (body, ct) = c.get("metrics").unwrap();
        assert_eq!(body, b"data");
        assert_eq!(ct, "text/plain");
    }

    #[test]
    fn expired_entry_returns_none() {
        let c = ResponseCache::new(CacheConfig {
            ttl: Duration::from_millis(1),
            enabled: true,
        });
        c.set("metrics", b"data".to_vec(), "text/plain");
        std::thread::sleep(Duration::from_millis(5));
        assert!(c.get("metrics").is_none());
    }

    #[test]
    fn disabled_cache_always_misses() {
        let c = ResponseCache::new(CacheConfig {
            ttl: Duration::from_secs(60),
            enabled: false,
        });
        c.set("metrics", b"data".to_vec(), "text/plain");
        assert!(c.get("metrics").is_none());
    }

    #[test]
    fn evict_expired_removes_stale_entries() {
        let c = ResponseCache::new(CacheConfig {
            ttl: Duration::from_millis(1),
            enabled: true,
        });
        c.set("a", b"1".to_vec(), "text/plain");
        c.set("b", b"2".to_vec(), "text/plain");
        std::thread::sleep(Duration::from_millis(5));
        c.evict_expired();
        assert_eq!(c.store.len(), 0);
    }
}
