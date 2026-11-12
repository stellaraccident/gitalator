pub mod git;
pub mod model;
pub mod store;
pub mod util;
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub fn error(s: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    s.into().into()
}
pub fn failpoint(name: &str) {
    if cfg!(debug_assertions) && std::env::var("GTL_FAILPOINT").ok().as_deref() == Some(name) {
        std::process::exit(86);
    }
}
