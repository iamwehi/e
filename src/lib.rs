//! `e`: a minimal security-audit agent harness.
//!
//! A small loop (agent), four general tools plus `finding` (tools, exec), a
//! short system prompt (prompt), DeepSeek streaming (provider) and state in
//! plain files under `.e/` (transcript, findings, notes).

pub mod agent;
pub mod config;
pub mod exec;
pub mod prompt;
pub mod provider;
pub mod term;
pub mod tools;
pub mod transcript;
