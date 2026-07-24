//! pr-review-core — reusable engine for an advisory AI PR reviewer.
//!
//! Pulls a pull request's diff, reviews it with a Claude model via OpenRouter,
//! and posts a line-anchored inline review plus an advisory summary comment.
//! Provider-agnostic across GitHub and Bitbucket. Bot identity and any extra
//! prompt are injected through [`config::Config`] so consumers (bot binaries)
//! supply their own branding.

pub mod agent;
pub mod backend;
pub mod blast;
pub mod command;
pub mod config;
pub mod deps;
pub mod diff;
pub mod llm;
pub mod prompt;
pub mod providers;
pub mod repo;
pub mod repo_config;
pub mod review;
pub mod structure;
pub mod webhook;

/// Clip a string to at most `n` characters (char-safe — never splits a UTF-8
/// codepoint). Used to keep API error bodies short in messages.
pub fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
