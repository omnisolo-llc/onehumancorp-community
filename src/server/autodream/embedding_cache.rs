// SPDX-License-Identifier: Apache-2.0
//! Local in-memory embedding response cache for AutoDream background consolidation.

use dashmap::DashMap;
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub response: String,
    pub created_at: Instant,
    pub expires_at: Instant,
}

pub struct LocalEmbeddingCache {
    entries: DashMap<String, CacheEntry>,
    ttl: Duration,
}

impl LocalEmbeddingCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: DashMap::new(),
            ttl,
        }
    }

    fn hash_prompt(&self, prompt: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(prompt.as_bytes());
        hex::encode(hasher.finalize())
    }

    pub fn get(&self, prompt: &str) -> Option<String> {
        let key = self.hash_prompt(prompt);
        let entry = self.entries.get(&key)?;
        if Instant::now() > entry.expires_at {
            return None;
        }
        Some(entry.response.clone())
    }

    pub fn set(&self, prompt: &str, response: &str) {
        let key = self.hash_prompt(prompt);
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
