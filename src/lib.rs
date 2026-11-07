pub mod backend;
pub mod cache;
pub mod cli;
pub mod db;
pub mod metadata;
pub mod model;
pub mod ops;
pub mod vfs;
pub mod workers;
pub mod workflow;
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub fn error(s: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    s.into().into()
}

/// Development-only crash injection for subprocess recovery tests.
pub fn failpoint(name: &str) {
    if cfg!(debug_assertions) && std::env::var("GTL_FAILPOINT").ok().as_deref() == Some(name) {
        std::process::exit(86);
    }
}

pub mod persistent;

pub mod activity;

pub mod ergonomics;

pub mod namespace;

pub mod config;
pub mod daemon;
pub mod logging;

mod interrupts;
