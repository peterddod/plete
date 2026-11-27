//! Session warm-up cache for agent profiles: a profile's `[warmup]` section,
//! where a warm-up is kept, and when it is stale. Nothing here runs a warm-up
//! or starts a task yet.

// Used by the build and `plete warmup` commands that follow.
#![allow(dead_code)]

pub mod cache;
pub mod config;
pub mod fresh;
pub mod git;

pub use config::{check, Config};
