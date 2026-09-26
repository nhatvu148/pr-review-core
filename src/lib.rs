#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unimplemented,
        clippy::todo,
        clippy::format_collect,
        clippy::print_stdout,
        clippy::print_stderr,
        reason = "a test asserts by panicking, stubs unreached trait methods, and builds fixtures the cheap way; the denials above are for production paths"
    )
)]

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
pub mod config_spec;
pub mod deps;
pub mod diff;
pub mod filereview;
pub mod findings;
pub mod llm;
pub mod mcp;
pub mod prompt;
pub mod providers;
pub mod queue;
pub mod repo;
pub mod repo_config;
pub mod review;
pub mod rules;
pub mod runlog;
pub mod structure;
pub mod suggest;
pub mod webhook;

/// Clip a string to at most `n` characters (char-safe — never splits a UTF-8
/// codepoint). Used to keep API error bodies short in messages.
pub fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod clip_tests {
    use super::clip;

    #[test]
    fn a_string_longer_than_the_limit_is_cut_to_it() {
        assert_eq!(clip("hello world", 5), "hello");
    }

    #[test]
    fn a_string_no_longer_than_the_limit_is_unchanged() {
        assert_eq!(clip("hi", 5), "hi");
        assert_eq!(clip("hi", 2), "hi");
    }

    #[test]
    fn zero_always_clips_to_empty() {
        assert_eq!(clip("anything", 0), "");
    }

    #[test]
    fn empty_input_stays_empty_regardless_of_limit() {
        assert_eq!(clip("", 10), "");
    }

    /// The limit counts chars, not bytes — multi-byte codepoints must never be
    /// split, which a byte-slice `&s[..n]` would do and panic on.
    #[test]
    fn the_limit_counts_chars_not_bytes_and_never_splits_one() {
        // Each of these is a multi-byte UTF-8 codepoint.
        assert_eq!(clip("日本語", 2), "日本");
        assert_eq!(clip("日本語", 3), "日本語");
        assert_eq!(clip("日本語", 10), "日本語");
    }
}
