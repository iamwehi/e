//! `e`: a minimal security-audit agent harness.
//!
//! A small loop (agent), four general tools plus `finding` (tools, exec), a
//! short system prompt (prompt), a streaming chat client (provider, Venice by
//! default), state in plain files under `.e/` (transcript, findings, notes)
//! the interactive prompt's `$EDITOR` key (editor), `e report` (report), and
//! the PoC checks behind `finding` and `e verify` / `e triage` (tree, verify).

pub mod agent;
pub mod config;
pub mod editor;
pub mod exec;
pub mod prompt;
pub mod provider;
pub mod report;
pub mod skills;
pub mod term;
pub mod tools;
pub mod transcript;
pub mod tree;
pub mod verify;
