// SPDX-License-Identifier: Apache-2.0
// Copyright (c) OmniSolo LLC.

//! Local single-tenant prompt optimization, caching, cost estimation, and token tracking utilities.

pub mod cache;
pub mod compression;
pub mod deduplication;
pub mod model_cost;
pub mod prompt_caching;
pub mod token_tracking;

pub use cache::{CacheEntry, CompressedEmbeddingCache, LocalEmbeddingCache};
pub use deduplication::{DeduplicationResult, DeduplicationState, RequestDeduplicator};
pub use token_tracking::TokenTracking;
