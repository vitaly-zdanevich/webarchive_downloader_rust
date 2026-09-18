pub mod alias_repair;
pub mod cdx;
mod css_refs;
pub mod download_refs;
pub mod downloader;
pub mod link_validation;
pub mod noise;
pub mod output_summary;
pub mod pathmap;
mod recovery;
mod retry;
pub mod rewrite;
pub mod soft_redirect;
pub mod wayback_client;

/// Default HTTP identity, kept in sync with Cargo release metadata.
pub const DEFAULT_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
