// SPDX-License-Identifier: Apache-2.0
// Copyright (c) OmniSolo LLC.

//! Local TTL and compressed prompt/embedding caches without external network deps.

use crate::prompt::compression;
use dashmap::DashMap;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub response: String,
    pub created_at: Instant,
    pub expires_at: Instant,
}

fn hash_prompt(prompt: &str) -> String {
    let digest = Sha256::digest(prompt.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

/// In-memory TTL cache keyed by SHA-256 hash of prompt text.
pub struct LocalEmbeddingCache {
    entries: DashMap<String, CacheEntry>,
    ttl: Duration,
}

impl LocalEmbeddingCache {
    pub fn new(ttl: Duration) -> Self {
        LocalEmbeddingCache {
            entries: DashMap::new(),
            ttl,
        }
    }

    pub fn get(&self, prompt: &str) -> Option<String> {
        let key = hash_prompt(prompt);
        if let Some(entry) = self.entries.get(&key) {
            if Instant::now() > entry.expires_at {
                return None;
            }
            Some(entry.response.clone())
        } else {
            None
        }
    }

    pub fn set(&self, prompt: &str, response: &str) {
        let key = hash_prompt(prompt);
        let now = Instant::now();
        self.entries.insert(
            key,
            CacheEntry {
                response: response.to_string(),
                created_at: now,
                expires_at: now + self.ttl,
            },
        );
    }

    pub fn prune(&self) -> usize {
        let now = Instant::now();
        let expired_keys: Vec<String> = self
            .entries
            .iter()
            .filter(|entry| now > entry.value().expires_at)
            .map(|entry| entry.key().clone())
            .collect();

        let pruned = expired_keys.len();
        for key in expired_keys {
            self.entries.remove(&key);
        }

        pruned
    }
}

/// Gzip+Base64 compressed variant of [`LocalEmbeddingCache`].
pub struct CompressedEmbeddingCache {
    entries: DashMap<String, CacheEntry>,
    ttl: Duration,
}

impl CompressedEmbeddingCache {
    pub fn new(ttl: Duration) -> Self {
        CompressedEmbeddingCache {
            entries: DashMap::new(),
            ttl,
        }
    }

    pub fn get(&self, prompt: &str) -> Option<String> {
        let key = hash_prompt(prompt);
        if let Some(entry) = self.entries.get(&key) {
            if Instant::now() > entry.expires_at {
                return None;
            }
            match compression::decompress_lossless(&entry.response) {
                Ok(decompressed) => Some(decompressed),
                Err(e) => {
                    tracing::error!("Failed to decompress cached response: {}", e);
                    None
                }
            }
        } else {
            None
        }
    }

    pub fn set(&self, prompt: &str, response: &str) {
        let key = hash_prompt(prompt);
        let now = Instant::now();

        match compression::compress_lossless(response) {
            Ok(compressed) => {
                self.entries.insert(
                    key,
                    CacheEntry {
                        response: compressed,
                        created_at: now,
                        expires_at: now + self.ttl,
                    },
                );
            }
            Err(e) => {
                tracing::error!("Failed to compress cache response: {}", e);
            }
        }
    }

    pub fn prune(&self) -> usize {
        let now = Instant::now();
        let expired_keys: Vec<String> = self
            .entries
            .iter()
            .filter(|entry| now > entry.value().expires_at)
            .map(|entry| entry.key().clone())
            .collect();

        let pruned = expired_keys.len();
        for key in expired_keys {
            self.entries.remove(&key);
        }

        pruned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_local_embedding_cache() {
        let cache = LocalEmbeddingCache::new(Duration::from_millis(50));

        cache.set("prompt1", "response1");
        assert_eq!(cache.get("prompt1"), Some("response1".to_string()));
        assert_eq!(cache.get("prompt2"), None);

        thread::sleep(Duration::from_millis(80));
        assert_eq!(cache.get("prompt1"), None);

        cache.set("prompt3", "response3");
        assert_eq!(cache.prune(), 1);
    }

    #[test]
    fn test_local_embedding_cache_get() {
        let cache = LocalEmbeddingCache::new(Duration::from_secs(60));
        let prompt = "test_prompt_get";
        let expected_response = "test_response_get";
        cache.set(prompt, expected_response);
        assert_eq!(cache.get(prompt), Some(expected_response.to_string()));
    }

    #[test]
    fn test_compressed_embedding_cache() {
        let cache = CompressedEmbeddingCache::new(Duration::from_millis(50));

        cache.set("prompt1", "response1");
        assert_eq!(cache.get("prompt1"), Some("response1".to_string()));

        thread::sleep(Duration::from_millis(80));
        assert_eq!(cache.get("prompt1"), None);
    }
}
