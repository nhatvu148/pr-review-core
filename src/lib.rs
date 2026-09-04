//! pr-review-core — reusable engine for an advisory AI PR reviewer.
//!
//! Pulls a pull request's diff, reviews it with a Claude model via OpenRouter,
//! and posts a line-anchored inline review plus an advisory summary comment.
//! Provider-agnostic across GitHub and Bitbucket. Bot identity and any extra
//! prompt are injected through [`config::Config`] so consumers (bot binaries)
//! supply their own branding.

/// This crate's version, for consumers that report which engine they are running.
///
/// A bot binary knows its own version but not its engine's, and "is the deployed
/// image current?" is otherwise answerable only by reading deploy logs — which cost
/// real time three separate times while shipping 0.11.0.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod agent;
pub mod backend;
pub mod blast;
pub mod changemap;
pub mod command;
pub mod complexity;
pub mod config;
pub mod deps;
pub mod diff;
pub mod llm;
pub mod prompt;
pub mod providers;
pub mod repo;
pub mod repo_config;
pub mod review;
pub mod runlog;
pub mod structure;
pub mod suggest;
pub mod webhook;

/// Clip a string to at most `n` characters (char-safe — never splits a UTF-8
/// codepoint). Used to keep API error bodies short in messages.
pub fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
