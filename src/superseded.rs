//! Skip a review whose commit is no longer the PR's head.
//!
//! With `REVIEW_ON_UPDATE=true` every push queues a full review, and nothing else
//! coalesces them: dedup is per *delivery*, and each push is its own delivery. An
//! agent pushing three fix commits in a row would buy three full reviews, two of
//! them for commits nobody will look at, and they hold the workers the review of
//! the latest head is waiting for.
//!
//! So the webhook records which commit triggered the review, and once the review
//! has a slot — after the queue wait, which is where a newer push lands — it asks
//! the provider for the current head and stops if that moved on. The newer push
//! queued its own review, so nothing goes unreviewed.
//!
//! **Only when `REVIEW_ON_UPDATE` is on.** That is what guarantees "the newer push
//! queued its own review". With it off a push queues nothing, so skipping the
//! open-time review because of one would leave the PR with no review at all. The
//! caller passes no commit in that case.
//!
//! **GitHub only.** The Bitbucket provider's `get_meta` leaves `head_sha` unset
//! (its inline comments need no commit id), so there is no head to compare against
//! and a Bitbucket review would only spend an API call to learn nothing.
//!
//! **Fails open.** No recorded commit (a `/review` comment, a direct review
//! request, the boot sweep, a job queued by an older build), a
//! provider error, or a timeout all mean "review it". A skipped review is only
//! ever one we positively know was superseded.

use std::time::Duration;

use crate::config::Config;
use crate::providers::Provider;
use serde::Deserialize;

/// Bound on the head lookup. It runs before a review that takes minutes, so a
/// slow provider costs little — but it must never be what stalls one.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// The head commit a GitHub `pull_request` payload was sent for.
///
/// Read here rather than through [`crate::webhook::parse_pull_request_event`],
/// whose `WebhookPr` does not carry it. `None` on any shape mismatch: a missing
/// SHA only turns the skip off, it must not reject the delivery.
///
/// ```
/// let body = br#"{"pull_request":{"head":{"sha":"abc123"}}}"#;
/// assert_eq!(pr_review_core::superseded::github_head_sha(body).as_deref(), Some("abc123"));
/// assert_eq!(pr_review_core::superseded::github_head_sha(b"{}"), None);
/// ```
pub fn github_head_sha(body: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Head {
        sha: Option<String>,
    }
    #[derive(Deserialize)]
    struct Pr {
        head: Option<Head>,
    }
    #[derive(Deserialize)]
    struct Event {
        pull_request: Option<Pr>,
    }
    let ev: Event = serde_json::from_slice(body).ok()?;
    non_empty(ev.pull_request?.head?.sha?)
}

fn non_empty(s: String) -> Option<String> {
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Whether two commit ids name the same commit.
///
/// ```
/// use pr_review_core::superseded::same_commit;
/// assert!(same_commit("0123456789abcdef0123456789abcdef01234567", "0123456789AB"));
/// assert!(!same_commit("01", "0123456789ab")); // too short to trust as a prefix
/// ```
///
/// A prefix match, not equality. GitHub sends full 40-char SHAs on both sides, so
/// today this is plain equality in practice. The prefix is for a provider that
/// abbreviates, as Bitbucket does (12 chars), should it ever supply a head to
/// compare. Anything under 7 chars is too short to trust as a prefix, so it is
/// compared exactly.
pub fn same_commit(a: &str, b: &str) -> bool {
    let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
    let (short, long) = if a.len() <= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    if short.len() < 7 {
        return short == long;
    }
    long.starts_with(short.as_str())
}

/// The newer head, when `triggered_for` is known to have been superseded.
///
/// `None` means review it: no recorded commit, a lookup that failed or timed out,
/// a PR with no head SHA, or a head that still matches.
///
/// A skip trusts that the newer push's own webhook arrived and queued a review.
/// If that delivery was lost, the newer head goes unreviewed until the next push
/// or the boot sweep. Knowing for sure needs shared per-PR state, which neither
/// an in-process path nor a job queue has.
///
/// ```no_run
/// # async fn demo(cfg: &pr_review_core::config::Config) {
/// // `None` for "no recorded commit": always review, without a lookup.
/// assert_eq!(pr_review_core::superseded::superseded_by(cfg, "github", "o/r", 1, None).await, None);
/// # }
/// ```
pub async fn superseded_by(
    cfg: &Config,
    provider: &str,
    repo: &str,
    pr: u64,
    triggered_for: Option<&str>,
) -> Option<String> {
    // Blank is "unknown", not a commit: `/review` takes `headSha` straight from a
    // request body, and an empty string would otherwise match no head and skip.
    let triggered_for = triggered_for.map(str::trim).filter(|s| !s.is_empty())?;
    let provider_impl = Provider::from_name(provider).ok()?;
    // One client for every lookup, so each review does not build its own pool and
    // TLS config. The provider call sets its own auth headers per request.
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(reqwest::Client::new);
    let lookup = provider_impl.get_meta(client, cfg, repo, pr);
    let head = match tokio::time::timeout(LOOKUP_TIMEOUT, lookup).await {
        Ok(Ok(meta)) => meta.head_sha?,
        Ok(Err(e)) => {
            tracing::warn!("{repo}#{pr}: head lookup failed ({e:#}); reviewing anyway");
            return None;
        }
        Err(_) => {
            tracing::warn!("{repo}#{pr}: head lookup timed out; reviewing anyway");
            return None;
        }
    };
    (!same_commit(triggered_for, &head)).then_some(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_sha_matches_its_own_abbreviation_either_way_round() {
        let full = "0123456789abcdef0123456789abcdef01234567";
        assert!(same_commit(full, "0123456789ab"));
        assert!(same_commit("0123456789AB", full));
    }

    #[test]
    fn a_different_commit_does_not_match() {
        assert!(!same_commit("0123456789ab", "fedcba987654"));
    }

    #[test]
    fn a_too_short_id_is_not_trusted_as_a_prefix() {
        // "01" prefixes half the commits in a busy repo.
        assert!(!same_commit("01", "0123456789ab"));
        assert!(same_commit("01", "01"));
    }

    #[test]
    fn a_github_payload_without_a_head_yields_none_rather_than_an_error() {
        assert_eq!(github_head_sha(br#"{"pull_request":{}}"#), None);
        assert_eq!(
            github_head_sha(br#"{"pull_request":{"head":{"sha":""}}}"#),
            None
        );
        assert_eq!(github_head_sha(b"not json"), None);
    }

    #[test]
    fn a_real_github_synchronize_shape_yields_the_head_not_the_base() {
        let body = br#"{"action":"synchronize","number":7,
            "pull_request":{"number":7,"head":{"sha":"headsha1"},"base":{"sha":"basesha1"}},
            "repository":{"full_name":"o/r"}}"#;
        assert_eq!(github_head_sha(body).as_deref(), Some("headsha1"));
    }

    #[tokio::test]
    async fn no_recorded_commit_always_reviews_without_a_lookup() {
        // A `/review` comment or the boot sweep has no commit to compare. It must
        // review, and must not even reach the provider (this config has no token).
        let mut cfg = Config::from_env();
        cfg.github_token.clear();
        assert_eq!(superseded_by(&cfg, "github", "o/r", 1, None).await, None);
    }

    #[tokio::test]
    async fn a_blank_commit_is_unknown_and_reviews() {
        let mut cfg = Config::from_env();
        cfg.github_token.clear();
        assert_eq!(
            superseded_by(&cfg, "github", "o/r", 1, Some("  ")).await,
            None
        );
    }

    #[tokio::test]
    async fn a_failed_lookup_reviews_rather_than_skipping() {
        // No GH_TOKEN: get_meta errors out before any request.
        let mut cfg = Config::from_env();
        cfg.github_token.clear();
        assert_eq!(
            superseded_by(&cfg, "github", "o/r", 1, Some("abc1234")).await,
            None
        );
    }
}
