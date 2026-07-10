//! Provider abstraction so the same review flow works against GitHub or
//! Bitbucket. Each provider knows how to fetch a PR's diff + metadata and post a
//! review (a summary comment + inline comments).

pub type ProviderName = &'static str;

/// Minimal PR context passed into the prompt and the providers.
#[derive(Debug, Clone)]
pub struct PrMeta {
    /// `owner/repo` (GitHub) or `workspace/repo` (Bitbucket).
    pub repo: String,
    /// PR number / id.
    pub pr: u64,
    pub title: Option<String>,
    pub base_branch: Option<String>,
    /// Head commit SHA — required by GitHub to anchor inline review comments.
    pub head_sha: Option<String>,
}

/// One inline comment anchored to a file + line on the new side of the diff.
#[derive(Debug, Clone)]
pub struct InlineComment {
    pub path: String,
    pub line: u64,
    pub body: String,
}

/// A rendered review ready to post: one summary comment plus zero or more
/// inline comments. Bodies are final markdown (the provider adds the dedupe
/// marker).
#[derive(Debug, Clone)]
pub struct ReviewPost {
    pub summary: String,
    pub inline: Vec<InlineComment>,
}
