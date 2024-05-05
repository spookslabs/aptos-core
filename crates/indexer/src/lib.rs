// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

// Copyright © Aptos Foundation

// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

// Increase recursion limit for `serde_json::json!` macro parsing
#![recursion_limit = "256"]

pub mod counters;
pub mod indexer;
pub mod runtime;

/// By default, skips test unless `INDEXER_DATABASE_URL` is set.
/// In CI, will explode if `INDEXER_DATABASE_URL` is NOT set.
pub fn should_skip_pg_tests() -> bool {
    if std::env::var("CIRCLECI").is_ok() {
        std::env::var("INDEXER_DATABASE_URL").expect("must set 'INDEXER_DATABASE_URL' in CI!");
    }
    if std::env::var("INDEXER_DATABASE_URL").is_ok() {
        false
    } else {
        aptos_logger::warn!("`INDEXER_DATABASE_URL` is not set: skipping indexer tests");
        true
    }
}
