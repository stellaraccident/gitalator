pub mod backend;
pub mod cache;
pub mod cli;
pub mod db;
pub mod model;
pub mod ops;
pub mod vfs;
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub fn error(s: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    s.into().into()
}
