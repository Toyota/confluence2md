//! Library root for confluence2md.
//!
//! Modules are split by responsibility:
//! while following Rust conventions (snake_case, idiomatic `Result` returns,
//! types co-located with the owning module).

pub mod confluence;
pub mod drawio;
pub mod html;
pub mod jira;
pub mod logger;
pub mod plantuml;
pub mod utils;
