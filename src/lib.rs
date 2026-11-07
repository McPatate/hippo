//! hippo — your AI memory as plain files you own, encrypted locally and backed up to a
//! Hugging Face bucket, so every agent can use it and none can keep it.

pub mod auth;
pub mod backup;
pub mod config;
pub mod crypto;
pub mod git;
pub mod import;
pub mod index;
pub mod mcp;
pub mod log;
pub mod memory;
pub mod remote;
pub mod safety;
pub mod secrets;
pub mod state;
pub mod statusline;
pub mod ui;
pub mod util;
pub mod vault;
