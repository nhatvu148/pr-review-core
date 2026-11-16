//! Boot reconciliation — find open PRs we accepted but never reviewed.
//!
//! A webhook review answers `202` and then runs detached. If the process dies
//! before it posts (crash, OOM, a drain that timed out), GitHub will not
//! redeliver — it already got its `202` — so the PR is silently never reviewed.
//! The shutdown drain covers the orderly case; this covers the rest.
//!
//! **The durable record is the posted comment.** Nothing else survives a
//! restart, and that's deliberate — reconciling against comments needs no store.
//!
//! Scope is explicit rather than discovered: it scans the repos named in
//! `RECONCILE_REPOS` and nothing else. Auto-discovering every repo an App is
//! installed on would make a boot — i.e. every deploy — capable of posting
//! reviews anywhere, which is a large blast radius for a background sweep nobody
//! is watching.
//!
//! On a platform that scales instances up and down, this runs per *instance*, so
//! a scale-up boots a sweep mid-day. The coverage check is what keeps that from
//! re-reviewing everything: a PR whose review already posted is skipped, so a
//! spurious boot is a few API calls, not a duplicate review.
//!
//! The module is a query. It returns what to review and starts nothing, so the
//! caller keeps its own concurrency limit, tracker and drain. The GitHub token
//! comes through [`GithubTokens`], so a bot running as a GitHub App can mint a
//! per-installation token while one on a personal token uses
//! [`ConfiguredToken`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::config::Config;
use crate::providers::is_bot_comment;
use crate::review::is_incomplete_review;

/// Where the sweep gets a GitHub token for a repo.
///
/// A trait rather than a token string because a GitHub App's token is per
/// installation, minted for the repo being read; a fixed token would be wrong for
/// every repo but one.
#[async_trait::async_trait]
pub trait GithubTokens: Send + Sync {
    /// A token that can read `repo`'s pull requests, commits and comments.
    async fn token(&self, cfg: &Config, repo: &str) -> Result<String>;
}

/// The token in the config (`GH_TOKEN`), for a deployment with no GitHub App.
///
/// ```
/// use pr_review_core::reconcile::{ConfiguredToken, GithubTokens};
/// # tokio_test_block_on(async {
/// let mut cfg = pr_review_core::config::Config::from_env();
/// cfg.github_token = String::new();
/// assert!(ConfiguredToken.token(&cfg, "o/r").await.is_err(), "no token is an error, not an anonymous call");
/// # });
/// # fn tokio_test_block_on<F: std::future::Future>(f: F) -> F::Output {
/// #     tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
/// # }
/// ```
pub struct ConfiguredToken;

#[async_trait::async_trait]
impl GithubTokens for ConfiguredToken {
    async fn token(&self, cfg: &Config, _repo: &str) -> Result<String> {
        // Refused here rather than sent empty: an anonymous call gets a 401 (or,
        // on a public repo, a rate limit of 60 an hour), which reads in the log
        // as the API refusing us rather than as the token never being set.
        if cfg.github_token.trim().is_empty() {
            anyhow::bail!("GH_TOKEN not set");
        }
        Ok(cfg.github_token.clone())
    }
}

/// A repo to sweep, and which provider it lives on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// `"github"` or `"bitbucket"`, as [`crate::providers::Provider::from_name`] takes them.
    pub provider: &'static str,
    /// `owner/repo` (GitHub) or `workspace/repo` (Bitbucket).
    pub repo: String,
}

/// Parse one `RECONCILE_REPOS` entry: `owner/repo`, `github:owner/repo`, or
/// `bitbucket:workspace/repo`. Unprefixed means GitHub.
///
/// An unknown prefix returns `None` rather than defaulting to GitHub — a typo'd
/// `bitbuckt:` silently sweeping the wrong provider would 404 every repo and
/// look like "nothing to reconcile".
pub(crate) fn parse_target(entry: &str) -> Option<Target> {
    let entry = entry.trim();
    let (provider, repo) = match entry.split_once(':') {
        Some(("github", r)) => ("github", r),
        Some(("bitbucket", r)) => ("bitbucket", r),
        Some(_) => return None,
        None => ("github", entry),
    };
    let repo = repo.trim();
    // owner/repo, exactly one slash, neither side empty.
    let mut parts = repo.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(Target {
        provider,
        repo: repo.to_string(),
    })
}

/// Whether one bot comment is evidence this PR **was reviewed** at its head.
///
/// Both halves matter and neither is redundant. `is_bot_comment` excludes other
/// people's comments; `is_incomplete_review` excludes our own comments that are not
/// reviews — the "⏳ Reviewing this PR…" placeholder and the failure notice that
/// replaces it.
///
/// Missing the second half is what let a dead review hide: `VinaText#49` posted its
/// placeholder 38 seconds after the head commit and then died on the turn cap, so
/// the newest bot comment was newer than the head and the sweep reported "nothing
/// missing" about the one PR it should have caught.
/// **Every provider scan must use this, not `is_bot_comment` directly.** There are
/// two — GitHub and Bitbucket — and the first version of this fix patched only the
/// GitHub one, leaving the identical bug live for Bitbucket. A predicate test
/// cannot catch that: it proves the rule, not that every caller applies it.
pub(crate) fn counts_as_coverage(cfg: &Config, body: &str) -> bool {
    is_bot_comment(cfg, body) && !is_incomplete_review(body)
}

/// A PR that looks unreviewed at its current head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreviewed {
    /// `"github"` or `"bitbucket"`.
    pub provider: &'static str,
    /// `owner/repo` (GitHub) or `workspace/repo` (Bitbucket).
    pub repo: String,
    /// The pull request number (GitHub) or id (Bitbucket).
    pub pr: u64,
}

/// Knobs, all bounded by default. Reconciliation runs at boot with nobody
/// watching, so every dimension that could run away has a ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileConfig {
    /// Repos from `RECONCILE_REPOS`. Empty disables the sweep.
    pub repos: Vec<Target>,
    /// Ignore PRs whose head commit is older than this. A PR untouched for days
    /// wasn't lost to a crash five minutes ago; it was never going to be
    /// reviewed, and reviewing it now is spam. `RECONCILE_LOOKBACK_MINS`.
    pub lookback_secs: i64,
    /// Hard cap on reviews started per boot. `RECONCILE_MAX_PRS`.
    pub max_prs: usize,
}

impl ReconcileConfig {
    /// Read `RECONCILE_REPOS` (comma-separated `owner/repo`, optionally prefixed
    /// `github:` or `bitbucket:`), `RECONCILE_LOOKBACK_MINS` (default 360) and
    /// `RECONCILE_MAX_PRS` (default 5).
    ///
    /// These are a consumer's settings, not the engine's: nothing in a review
    /// reads them, which is why they are not in the `Config` reference. A
    /// malformed entry is logged and skipped rather than swept on the wrong
    /// provider.
    ///
    /// ```
    /// let rc = pr_review_core::reconcile::ReconcileConfig::from_env();
    /// // Off unless RECONCILE_REPOS names something.
    /// assert_eq!(rc.enabled(), !rc.repos.is_empty() && rc.max_prs > 0);
    /// ```
    pub fn from_env() -> Self {
        let raw = std::env::var("RECONCILE_REPOS").unwrap_or_default();
        let repos = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter_map(|s| match parse_target(s) {
                Some(t) => Some(t),
                None => {
                    tracing::error!(
                        "RECONCILE_REPOS entry {s:?} is not owner/repo (optionally \
                         github:/bitbucket:-prefixed) — skipping it"
                    );
                    None
                }
            })
            .collect();
        let lookback_mins = env_num("RECONCILE_LOOKBACK_MINS").unwrap_or(360);
        let max_prs = env_num("RECONCILE_MAX_PRS").unwrap_or(5) as usize;
        Self {
            repos,
            // Saturating: a huge value means "no limit", not a wrapped negative window.
            lookback_secs: lookback_mins.saturating_mul(60),
            max_prs,
        }
    }

    /// Whether a sweep would scan anything: at least one repo and a cap above 0.
    pub fn enabled(&self) -> bool {
        !self.repos.is_empty() && self.max_prs > 0
    }
}

fn env_num(key: &str) -> Option<i64> {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|&n| n >= 0)
}

/// Seconds since the epoch, from the RFC 3339 timestamps GitHub returns
/// (`2026-08-02T01:06:07Z`).
///
/// Hand-rolled because this crate has no date dependency and one timestamp
/// format is not worth adding one. Accepts a trailing `Z` or a `+00:00` offset
/// (what Bitbucket sends) and ignores fractional seconds; **any other offset is
/// rejected** rather than silently mistreated as UTC, which would shift a
/// comparison by hours and flip a coverage decision.
pub(crate) fn epoch_from_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    // Everything after the seconds field is optional fractional seconds followed
    // by a zone. Trim the fraction; whatever remains must be an explicit UTC
    // marker. A *missing* zone is rejected rather than assumed: it's ambiguous,
    // both providers always send one, and guessing wrong shifts a coverage
    // comparison by hours.
    // `get`, not indexing: byte 19 can fall inside a multi-byte character in a
    // malformed timestamp, and a panic here would take the boot sweep down.
    let zone = s
        .get(19..)?
        .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
    if !matches!(zone, "Z" | "z" | "+00:00" | "-00:00") {
        return None;
    }

    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec)
}

/// Days since 1970-01-01 for a civil date. Howard Hinnant's `days_from_civil`,
/// which is exact for the proleptic Gregorian calendar — no leap-year edge case
/// to get subtly wrong once a year.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Which comments the GitHub fetch may restrict itself to.
///
/// `Some(head)` narrows the request to comments touched since the head commit —
/// cheap, and enough to answer "is there a review *newer than the head*".
///
/// `None` when `REVIEW_ON_UPDATE` is off, and that exception is load-bearing.
/// With the flag off the bot never re-edits its marker comment after a push, so
/// the comment's `updated_at` stays *behind* the head and GitHub's `since`
/// filter — which matches on `updated_at` — would exclude the very review we're
/// looking for. The fetch would come back empty, the PR would read as never
/// reviewed, and reconciliation would post the duplicate that branch exists to
/// prevent.
pub(crate) fn comment_window(review_on_update: bool, head_iso: &str) -> Option<&str> {
    review_on_update.then_some(head_iso)
}

/// When a comment last said anything — the later of created and updated.
///
/// Always the later of the two: the engine edits its marker comment in place, so
/// the edit is the review, and `created_at` only marks the first one ever.
pub(crate) fn comment_time(created: &str, updated: Option<&str>) -> Option<i64> {
    let c = epoch_from_rfc3339(created);
    let u = updated.and_then(epoch_from_rfc3339);
    match (c, u) {
        (Some(c), Some(u)) => Some(c.max(u)),
        (Some(c), None) => Some(c),
        (None, u) => u,
    }
}

/// Should this PR be reviewed, given what the bot would have done itself?
///
/// With `REVIEW_ON_UPDATE=false` the webhook path deliberately does **not**
/// re-review a push, so neither may this: a PR that carries any review of ours
/// is left alone, and only a PR with none at all counts as lost. Reconciliation
/// recovers work that was dropped; it must not quietly re-enable a mode the
/// operator turned off.
pub(crate) fn should_reconcile(
    head_committed_at: i64,
    newest_bot_comment_at: Option<i64>,
    review_on_update: bool,
) -> bool {
    if !review_on_update {
        return newest_bot_comment_at.is_none();
    }
    needs_review(head_committed_at, newest_bot_comment_at)
}

/// Has this PR been reviewed *at its current head*?
///
/// The marker comment carries no commit SHA, so coverage is a timestamp
/// comparison: a review posted after the head commit landed is a review *of*
/// that commit. Ties count as covered — equal timestamps mean the review and
/// the push are the same second, and double-reviewing is the worse error.
pub(crate) fn needs_review(head_committed_at: i64, newest_bot_comment_at: Option<i64>) -> bool {
    match newest_bot_comment_at {
        None => true,
        Some(t) => t < head_committed_at,
    }
}

/// Is this PR recent enough to be plausibly ours to finish?
pub(crate) fn within_lookback(head_committed_at: i64, now: i64, lookback_secs: i64) -> bool {
    now.saturating_sub(head_committed_at) <= lookback_secs
}

// ---------------------------------------------------------------------------
// HTTP layer
// ---------------------------------------------------------------------------

/// The HTTP status of a failed request, attached as the error's root cause.
///
/// Exists so callers can separate "this resource isn't there" (a 404 — a fork we
/// can't read, a deleted branch) from "the API is refusing us" (401/403/429/5xx).
/// Both make a PR unreconcilable, but only one of them is worth waking up for,
/// and logging them identically is how an outage gets mistaken for a quiet day.
#[derive(Debug)]
struct HttpStatus(u16);

impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}", self.0)
    }
}

impl std::error::Error for HttpStatus {}

/// `true` when the request failed because the thing simply isn't there.
fn is_not_found(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|c| c.downcast_ref::<HttpStatus>().is_some_and(|s| s.0 == 404))
}

#[derive(Deserialize)]
struct PullListItem {
    number: u64,
    #[serde(default)]
    draft: bool,
    head: PullHead,
}

#[derive(Deserialize)]
struct PullHead {
    sha: String,
}

#[derive(Deserialize)]
struct CommitResp {
    commit: CommitInner,
}

#[derive(Deserialize)]
struct CommitInner {
    committer: CommitActor,
}

#[derive(Deserialize)]
struct CommitActor {
    date: String,
}

#[derive(Deserialize)]
struct IssueComment {
    body: Option<String>,
    created_at: String,
    /// The engine **edits its existing marker comment** rather than posting a new
    /// one, so `created_at` is frozen at the first review this PR ever got. Using
    /// it alone means any PR reviewed once and then pushed to looks unreviewed on
    /// every single boot — an unbounded duplicate-review loop.
    updated_at: Option<String>,
}

/// The numbers of every currently-open PR in `targets`, keyed by
/// `(provider, repo)`.
///
/// Exists because a run log answers "what was reviewed", never "what is open" —
/// so a queue folded from it lists PRs that have since merged or closed, and puts
/// them at the top when their last review said BLOCK. Measured on a real log:
/// every P0 in the first production run was a closed PR. It needs a provider
/// token, so a consumer calls it with its own [`GithubTokens`].
///
/// A repo that errors is **omitted from the map**, not recorded as having no open
/// PRs. The difference matters: "no open PRs" would silently drop every row for
/// that repo out of the queue, turning an auth failure into an empty section that
/// looks like good news. The caller can tell the two apart and say so.
pub async fn open_pr_numbers(
    cfg: &Arc<Config>,
    tokens: &dyn GithubTokens,
    targets: &[Target],
) -> HashMap<(String, String), HashSet<u64>> {
    let http = reqwest::Client::builder()
        .user_agent(cfg.user_agent.clone())
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_default();

    let mut out = HashMap::new();
    for t in targets {
        let listed = match t.provider {
            "bitbucket" => bb_open_numbers(&http, cfg, &t.repo).await,
            _ => gh_open_numbers(&http, cfg, tokens, &t.repo).await,
        };
        match listed {
            Ok(nums) => {
                out.insert((t.provider.to_string(), t.repo.clone()), nums);
            }
            // Same posture as the boot sweep: one unreachable repo must not take
            // the whole answer down.
            Err(e) => tracing::error!("queue: skipping {}:{} — {e:#}", t.provider, t.repo),
        }
    }
    out
}

/// Open PR numbers on GitHub. Drafts are included: unlike the reconcile sweep,
/// which asks "should this have been reviewed?", the queue asks "is this PR still
/// live?" — and a draft is.
async fn gh_open_numbers(
    http: &reqwest::Client,
    cfg: &Arc<Config>,
    tokens: &dyn GithubTokens,
    repo: &str,
) -> Result<HashSet<u64>> {
    let token = tokens.token(cfg, repo).await?;
    let mut out = HashSet::new();
    for page in 1..=4 {
        let pulls: Vec<PullListItem> = get_json(
            http,
            &token,
            cfg,
            &format!(
                "{api}/repos/{repo}/pulls\
                 ?state=open&per_page=100&page={page}",
                api = api_base(cfg)
            ),
        )
        .await
        .with_context(|| format!("list open PRs for {repo}"))?;
        let n = pulls.len();
        out.extend(pulls.into_iter().map(|p| p.number));
        // A short page proves the end of the list.
        if n < 100 {
            return Ok(out);
        }
    }
    // Fell out of the loop with every page full: there are more. Said out loud
    // for the same reason the Bitbucket path says it — a set that is short here
    // reports a genuinely open PR as closed, and silence would make that
    // indistinguishable from a correct answer.
    tracing::warn!(
        "queue: {repo} has more open PRs than we'll page through; some may be reported as closed"
    );
    Ok(out)
}

/// Open PR ids on Bitbucket, following `next` to the end of the list.
///
/// Paging is not optional here. A single page would cap the set, and a *capped*
/// set is worse than no set at all in this direction: an open PR missing from it
/// is reported as **closed** and drops out of the queue — the exact failure the
/// queue exists to prevent, arriving via pagination instead of staleness. The
/// GitHub side can bound its loop because a short page proves the end; Bitbucket
/// tells us directly with `next`, so there is no reason to guess.
async fn bb_open_numbers(
    http: &reqwest::Client,
    cfg: &Arc<Config>,
    repo: &str,
) -> Result<HashSet<u64>> {
    let auth = bb_auth(cfg)?;
    let base = cfg.bitbucket_api_base.trim_end_matches('/');
    let mut url = format!("{base}/repositories/{repo}/pullrequests?state=OPEN&pagelen=50");
    let mut out = HashSet::new();
    // Bounded so a pathological repo cannot spin forever; 40 pages is 2000 open
    // PRs, and the warning says the set is short rather than letting it look
    // complete.
    for _ in 0..40 {
        let page: BbPage<BbPull> = bb_get(http, &auth, cfg, &url)
            .await
            .with_context(|| format!("list open PRs for {repo}"))?;
        out.extend(page.values.iter().map(|p| p.id));
        match page.next {
            Some(n) if !n.is_empty() => url = n,
            _ => return Ok(out),
        }
    }
    tracing::warn!(
        "queue: {repo} has more open PRs than we'll page through; some may be reported as closed"
    );
    Ok(out)
}

/// Scan the configured repos and return the PRs that still look unreviewed.
///
/// Returns the list rather than starting reviews, so the caller owns the
/// concurrency limit, the tracker, and the drain — this module stays a query.
///
/// Errors on one repo are logged and skipped: a boot sweep must never be able to
/// stop the server from starting.
pub async fn find_unreviewed(
    cfg: &Arc<Config>,
    tokens: &dyn GithubTokens,
    rc: &ReconcileConfig,
    now: i64,
) -> Vec<Unreviewed> {
    let http = reqwest::Client::builder()
        .user_agent(cfg.user_agent.clone())
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_default();

    let mut found = Vec::new();
    for t in &rc.repos {
        if found.len() >= rc.max_prs {
            tracing::warn!(
                "reconcile: stopping at the {} PR cap (RECONCILE_MAX_PRS) — repos from {} \
                 onward were not scanned, so a review may still be missing there",
                rc.max_prs,
                t.repo
            );
            break;
        }
        let budget = rc.max_prs - found.len();
        let scanned = match t.provider {
            "bitbucket" => scan_bitbucket(&http, cfg, rc, &t.repo, now, budget).await,
            _ => scan_github(&http, cfg, tokens, rc, &t.repo, now, budget).await,
        };
        match scanned {
            Ok(mut prs) => found.append(&mut prs),
            Err(e) => tracing::error!("reconcile: skipping {}:{} — {e:#}", t.provider, t.repo),
        }
    }
    found
}

async fn scan_github(
    http: &reqwest::Client,
    cfg: &Arc<Config>,
    tokens: &dyn GithubTokens,
    rc: &ReconcileConfig,
    repo: &str,
    now: i64,
    budget: usize,
) -> Result<Vec<Unreviewed>> {
    let token = tokens.token(cfg, repo).await?;
    let pulls: Vec<PullListItem> = get_json(
        http,
        &token,
        cfg,
        &format!(
            "{api}/repos/{repo}/pulls\
             ?state=open&sort=updated&direction=desc&per_page=30",
            api = api_base(cfg)
        ),
    )
    .await
    .with_context(|| format!("list open PRs for {repo}"))?;

    let mut out = Vec::new();
    for p in pulls {
        if out.len() >= budget {
            break;
        }
        // A draft was never going to be auto-reviewed, so its absence of a
        // review isn't evidence of a loss.
        if p.draft {
            continue;
        }
        let (head_iso, head_at) = match head_commit_date(http, &token, cfg, repo, &p.head.sha).await
        {
            Ok(t) => t,
            Err(e) if is_not_found(&e) => {
                tracing::info!(
                    "reconcile: skipping {repo}#{} — head commit {} is not readable",
                    p.number,
                    p.head.sha
                );
                continue;
            }
            Err(e) => {
                tracing::error!("reconcile: {repo}#{} — {e:#}", p.number);
                continue;
            }
        };
        if !within_lookback(head_at, now, rc.lookback_secs) {
            continue;
        }
        let window = comment_window(cfg.review_on_update, &head_iso);
        let newest = match newest_bot_comment_at(http, &token, cfg, repo, p.number, window).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("reconcile: {repo}#{} comments — {e:#}", p.number);
                continue;
            }
        };
        if should_reconcile(head_at, newest, cfg.review_on_update) {
            tracing::warn!(
                "reconcile: {repo}#{} has no review for its current head — \
                 starting one (a previous review was lost)",
                p.number
            );
            out.push(Unreviewed {
                provider: "github",
                repo: repo.to_string(),
                pr: p.number,
            });
        }
    }
    Ok(out)
}

/// The head commit's date, both as GitHub sent it (usable verbatim as a `since`
/// filter) and as epoch seconds (for the comparison).
async fn head_commit_date(
    http: &reqwest::Client,
    token: &str,
    cfg: &Arc<Config>,
    repo: &str,
    sha: &str,
) -> Result<(String, i64)> {
    let c: CommitResp = get_json(
        http,
        token,
        cfg,
        &format!("{api}/repos/{repo}/commits/{sha}", api = api_base(cfg)),
    )
    .await?;
    let iso = c.commit.committer.date;
    let at = epoch_from_rfc3339(&iso).with_context(|| format!("unparsable commit date {iso:?}"))?;
    Ok((iso, at))
}

/// When we last said something on this PR at or after `since`, if ever.
///
/// Scoped with `since` (when [`comment_window`] allows) rather than fetched
/// wholesale, because this endpoint
/// returns comments **oldest first** and cannot be reordered — `sort`/`direction`
/// belong to the repo-level comments endpoint, not this one, and are silently
/// ignored here. Without a filter, page 1 of a PR with >100 comments is the
/// *oldest* 100, our review would be invisible, and reconciliation would post a
/// duplicate on the busiest PRs — exactly where it hurts most.
///
/// `since` filters on `updated_at`, and the comment's time here is the later of
/// created/updated — the engine edits its marker comment in place, so the edit
/// *is* the review.
///
/// `since = None` reads from the oldest comment forward and stops at the first
/// one of ours. That's the [`comment_window`] case: the question becomes "does
/// any review of ours exist at all", for which the oldest match answers just as
/// well as the newest, and no server-side filter can hide it.
async fn newest_bot_comment_at(
    http: &reqwest::Client,
    token: &str,
    cfg: &Arc<Config>,
    repo: &str,
    pr: u64,
    since: Option<&str>,
) -> Result<Option<i64>> {
    const PER_PAGE: usize = 100;
    /// Enough for 500 comments *since the head commit*. Past that, treating the
    /// PR as reviewed is the safe answer — see the early return below.
    const MAX_PAGES: usize = 5;

    let mut newest: Option<i64> = None;
    for page in 1..=MAX_PAGES {
        let mut url = format!(
            "{api}/repos/{repo}/issues/{pr}/comments\
             ?per_page={PER_PAGE}&page={page}",
            api = api_base(cfg)
        );
        if let Some(since) = since {
            url.push_str(&format!("&since={since}"));
        }
        let comments: Vec<IssueComment> = get_json(http, token, cfg, &url).await?;
        let count = comments.len();
        for c in &comments {
            let body = c.body.as_deref().unwrap_or("");
            // A placeholder or a failure notice is NOT a review, and counting one
            // as coverage is what let a dead review hide. `VinaText#49` posted its
            // "Reviewing…" placeholder 38s after the head commit, then died on the
            // turn cap: the comment was newer than the head, so the next sweep
            // reported "nothing missing" about the exact PR it should have caught.
            if counts_as_coverage(cfg, body) {
                if let Some(t) = comment_time(&c.created_at, c.updated_at.as_deref()) {
                    newest = Some(newest.map_or(t, |n: i64| n.max(t)));
                    // Unfiltered: any review of ours answers the question, and
                    // the rest of the pages can't change it.
                    if since.is_none() {
                        return Ok(newest);
                    }
                }
            }
        }
        if count < PER_PAGE {
            return Ok(newest);
        }
    }
    // More pages than we're willing to read. Claiming "no review found" here
    // would post a duplicate on the noisiest PR in the repo, so decline instead:
    // a far-future timestamp reads as covered, and the log says why.
    if newest.is_none() {
        // Say which window we actually read: with `since` we saw only comments
        // touched after the head commit; without it we read the oldest N of the
        // whole thread. "Since its head commit" would be wrong in that branch.
        let scope = if since.is_some() {
            "since its head commit"
        } else {
            "in this PR's history"
        };
        tracing::warn!(
            "reconcile: {repo}#{pr} has more than {} comments {scope} without one of \
             ours; treating it as reviewed rather than risking a duplicate",
            PER_PAGE * MAX_PAGES
        );
        return Ok(Some(i64::MAX));
    }
    Ok(newest)
}

/// The GitHub API base, so the sweep reaches GitHub Enterprise when `GH_API_BASE`
/// points there, as every other GitHub call in this crate does.
fn api_base(cfg: &Config) -> &str {
    cfg.github_api_base.trim_end_matches('/')
}

async fn get_json<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    token: &str,
    cfg: &Arc<Config>,
    url: &str,
) -> Result<T> {
    let res = http
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", &cfg.user_agent)
        .send()
        .await
        .context("send")?;
    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(
            anyhow::Error::new(HttpStatus(status.as_u16())).context(format!(
                "{status}: {}",
                body.chars().take(200).collect::<String>()
            )),
        );
    }
    serde_json::from_str(&body).context("decode")
}

// ---------------------------------------------------------------------------
// Bitbucket
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BbPage<T> {
    #[serde(default = "Vec::new")]
    values: Vec<T>,
    #[serde(default)]
    next: Option<String>,
}

#[derive(Deserialize)]
struct BbPull {
    id: u64,
    source: BbSource,
    /// Bitbucket Cloud marks draft PRs too. `#[serde(default)]` because the
    /// field is recent — an older/self-hosted response without it must read as
    /// "not a draft" rather than failing the whole page.
    #[serde(default)]
    draft: bool,
}

#[derive(Deserialize)]
struct BbSource {
    commit: BbCommitRef,
    /// The repo the head commit actually lives in. Differs from the PR's own
    /// repo for a fork, and the commit endpoint is repo-scoped — looking a fork's
    /// SHA up under the destination repo is a guaranteed 404.
    #[serde(default)]
    repository: Option<BbRepo>,
}

#[derive(Deserialize)]
struct BbRepo {
    full_name: Option<String>,
}

#[derive(Deserialize)]
struct BbCommitRef {
    hash: String,
}

#[derive(Deserialize)]
struct BbCommit {
    date: String,
}

#[derive(Deserialize)]
struct BbComment {
    content: Option<BbContent>,
    created_on: String,
    /// See `IssueComment::updated_at` — same edit-in-place behaviour.
    updated_on: Option<String>,
    /// Bitbucket keeps deleted comments in the list with their body removed.
    #[serde(default)]
    deleted: bool,
}

#[derive(Deserialize)]
struct BbContent {
    raw: Option<String>,
}

/// `Basic base64(email:token)` — Bitbucket's app-password/API-token scheme.
fn bb_auth(cfg: &Config) -> Result<String> {
    use base64::Engine as _;
    if cfg.bitbucket_email.trim().is_empty() || cfg.bitbucket_token.trim().is_empty() {
        anyhow::bail!("BB_EMAIL / BB_API_TOKEN not set");
    }
    let raw = format!("{}:{}", cfg.bitbucket_email, cfg.bitbucket_token);
    Ok(format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw)
    ))
}

async fn scan_bitbucket(
    http: &reqwest::Client,
    cfg: &Arc<Config>,
    rc: &ReconcileConfig,
    repo: &str,
    now: i64,
    budget: usize,
) -> Result<Vec<Unreviewed>> {
    let auth = bb_auth(cfg)?;
    let base = cfg.bitbucket_api_base.trim_end_matches('/');
    let page: BbPage<BbPull> = bb_get(
        http,
        &auth,
        cfg,
        &format!(
            "{base}/repositories/{repo}/pullrequests\
             ?state=OPEN&pagelen=30&sort=-updated_on"
        ),
    )
    .await
    .with_context(|| format!("list open PRs for {repo}"))?;

    let mut out = Vec::new();
    for p in page.values {
        if out.len() >= budget {
            break;
        }
        // Same rule as GitHub: a draft was never going to be auto-reviewed, so
        // no review on one isn't evidence that we lost anything.
        if p.draft {
            continue;
        }
        // Fork PRs carry their head commit in the source repo, not this one.
        let commit_repo = p
            .source
            .repository
            .as_ref()
            .and_then(|r| r.full_name.as_deref())
            .unwrap_or(repo);
        let head_at = match bb_commit_date(
            http,
            &auth,
            cfg,
            base,
            commit_repo,
            &p.source.commit.hash,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                // Not an error worth shouting about on every boot: a deleted
                // branch or a fork we can't read leaves the head unknowable,
                // and an unknowable head means we can't prove a review is
                // missing. Skipping is the safe answer either way.
                tracing::info!(
                    "reconcile: skipping {repo}#{} — can't read its head commit \
                         ({}): {e:#}",
                    p.id,
                    p.source.commit.hash
                );
                continue;
            }
        };
        if !within_lookback(head_at, now, rc.lookback_secs) {
            continue;
        }
        let newest = match bb_newest_bot_comment_at(http, &auth, cfg, base, repo, p.id).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("reconcile: {repo}#{} comments — {e:#}", p.id);
                continue;
            }
        };
        if should_reconcile(head_at, newest, cfg.review_on_update) {
            tracing::warn!(
                "reconcile: {repo}#{} has no review for its current head — \
                 starting one (a previous review was lost)",
                p.id
            );
            out.push(Unreviewed {
                provider: "bitbucket",
                repo: repo.to_string(),
                pr: p.id,
            });
        }
    }
    Ok(out)
}

async fn bb_commit_date(
    http: &reqwest::Client,
    auth: &str,
    cfg: &Arc<Config>,
    base: &str,
    repo: &str,
    hash: &str,
) -> Result<i64> {
    let c: BbCommit = bb_get(
        http,
        auth,
        cfg,
        &format!("{base}/repositories/{repo}/commit/{hash}"),
    )
    .await?;
    epoch_from_rfc3339(&c.date).with_context(|| format!("unparsable commit date {:?}", c.date))
}

/// Newest comment of ours on this PR, if any.
///
/// Bitbucket has no `since` filter. Every page up to the bound is read and the
/// latest created-or-updated time among our coverage comments wins: the engine
/// edits its summary comment in place, so the comment with the newest activity
/// can be the oldest one created, and stopping at the first match in creation
/// order read a re-reviewed PR as last reviewed before its head. Running out of
/// pages before the end is treated as covered rather than risking a duplicate
/// review, same as the GitHub path, since a newer edit may be on a page not read.
async fn bb_newest_bot_comment_at(
    http: &reqwest::Client,
    auth: &str,
    cfg: &Arc<Config>,
    base: &str,
    repo: &str,
    pr: u64,
) -> Result<Option<i64>> {
    const MAX_PAGES: usize = 5;
    let mut url = format!(
        "{base}/repositories/{repo}/pullrequests/{pr}/comments\
         ?pagelen=100&sort=-created_on"
    );
    let mut newest: Option<i64> = None;
    for _ in 0..MAX_PAGES {
        let page: BbPage<BbComment> = bb_get(http, auth, cfg, &url).await?;
        for c in &page.values {
            if c.deleted {
                continue;
            }
            let body = c
                .content
                .as_ref()
                .and_then(|x| x.raw.as_deref())
                .unwrap_or("");
            // Same predicate as the GitHub scan. Using raw `is_bot_comment` here
            // was the identical bug one provider over: a Bitbucket PR whose review
            // died still showed a placeholder newer than its head, and the sweep
            // read it as coverage.
            if counts_as_coverage(cfg, body) {
                if let Some(t) = comment_time(&c.created_on, c.updated_on.as_deref()) {
                    newest = Some(newest.map_or(t, |n: i64| n.max(t)));
                }
            }
        }
        match page.next {
            Some(n) if !n.is_empty() => url = n,
            _ => return Ok(newest),
        }
    }
    tracing::warn!(
        "reconcile: {repo}#{pr} has more comments than we'll page through; \
         treating it as reviewed rather than risking a duplicate"
    );
    Ok(Some(i64::MAX))
}

async fn bb_get<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    auth: &str,
    cfg: &Arc<Config>,
    url: &str,
) -> Result<T> {
    let res = http
        .get(url)
        .header("Authorization", auth)
        .header("Accept", "application/json")
        .header("User-Agent", &cfg.user_agent)
        .send()
        .await
        .context("send")?;
    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(
            anyhow::Error::new(HttpStatus(status.as_u16())).context(format!(
                "{status}: {}",
                body.chars().take(200).collect::<String>()
            )),
        );
    }
    serde_json::from_str(&body).context("decode")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine edits its summary in place, so on a re-reviewed PR the bot
    /// comment with the newest activity is the oldest one created. Stopping at
    /// the first match in creation order read that PR as last reviewed at 03:00
    /// instead of 05:00, before a head pushed at 04:00.
    #[tokio::test]
    async fn the_bitbucket_scan_takes_the_newest_edit_not_the_newest_creation() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let srv = MockServer::start().await;
        let cfg = Arc::new(Config::from_env());
        let ours = |s: &str| format!("{s}\n\n_{}_", cfg.comment_marker);
        Mock::given(method("GET"))
            .and(path("/repositories/w/r/pullrequests/1/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "values": [
                    { "content": { "raw": ours("a reply") },
                      "created_on": "2026-08-02T03:00:00Z" },
                    { "content": { "raw": "someone else" },
                      "created_on": "2026-08-02T02:00:00Z" },
                    { "content": { "raw": ours("the summary") },
                      "created_on": "2026-08-02T01:00:00Z",
                      "updated_on": "2026-08-02T05:00:00Z" }
                ]
            })))
            .mount(&srv)
            .await;
        let got = bb_newest_bot_comment_at(
            &reqwest::Client::new(),
            "Bearer x",
            &cfg,
            &srv.uri(),
            "w/r",
            1,
        )
        .await
        .expect("the scan runs");
        assert_eq!(got, epoch_from_rfc3339("2026-08-02T05:00:00Z"));
    }

    #[test]
    fn a_malformed_timestamp_with_non_ascii_is_rejected_not_a_panic() {
        assert_eq!(epoch_from_rfc3339("2026-08-02T01:06:0é+00:00"), None);
        assert_eq!(epoch_from_rfc3339("2026-08-02T01:06:07é"), None);
    }

    #[test]
    fn parses_github_timestamps() {
        assert_eq!(epoch_from_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch_from_rfc3339("2026-08-02T01:06:07Z"), Some(1785632767));
        // Bitbucket's offset form and fractional seconds must agree with `Z`.
        assert_eq!(
            epoch_from_rfc3339("2026-08-02T01:06:07+00:00"),
            epoch_from_rfc3339("2026-08-02T01:06:07Z")
        );
        assert_eq!(
            epoch_from_rfc3339("2026-08-02T01:06:07.123456Z"),
            epoch_from_rfc3339("2026-08-02T01:06:07Z")
        );
    }

    /// A non-UTC offset silently treated as UTC would shift coverage by hours
    /// and could re-review a PR that was reviewed. Reject instead.
    #[test]
    fn rejects_offsets_and_junk() {
        for s in [
            "2026-08-02T01:06:07+07:00",
            "2026-08-02T01:06:07-05:00",
            // No zone at all is ambiguous, not implicitly UTC.
            "2026-08-02T01:06:07",
            "2026-08-02T01:06:07.500",
            "",
            "not a date",
            "2026-08-02",
        ] {
            assert_eq!(epoch_from_rfc3339(s), None, "{s}");
        }
    }

    /// Leap days are the classic off-by-one in hand-rolled date maths.
    #[test]
    fn handles_leap_days_and_ordering() {
        let feb29 = epoch_from_rfc3339("2024-02-29T00:00:00Z").unwrap();
        let mar1 = epoch_from_rfc3339("2024-03-01T00:00:00Z").unwrap();
        assert_eq!(mar1 - feb29, 86_400);
        // 2100 is not a leap year (the /100 rule).
        let feb28 = epoch_from_rfc3339("2100-02-28T00:00:00Z").unwrap();
        let mar1_2100 = epoch_from_rfc3339("2100-03-01T00:00:00Z").unwrap();
        assert_eq!(mar1_2100 - feb28, 86_400);
    }

    /// The engine edits its marker comment in place, so the edit time is the
    /// review time. Ignoring it re-reviews every pushed-to PR on every boot.
    #[test]
    fn comment_time_prefers_the_edit() {
        let created = "2026-08-02T02:26:15Z";
        let updated = "2026-08-02T03:18:25Z";
        assert_eq!(
            comment_time(created, Some(updated)),
            epoch_from_rfc3339(updated)
        );
        assert_eq!(comment_time(created, None), epoch_from_rfc3339(created));
        // An edit older than creation shouldn't move it backwards.
        assert_eq!(
            comment_time(updated, Some(created)),
            epoch_from_rfc3339(updated)
        );
    }

    /// The `since` filter matches on `updated_at`. With REVIEW_ON_UPDATE off the
    /// bot never re-edits its comment after a push, so that filter would hide
    /// the review and the PR would read as never-reviewed — firing the duplicate
    /// the off-branch exists to prevent. Narrowing is only safe with the flag on.
    #[test]
    fn comment_window_is_unfiltered_when_update_reviews_are_off() {
        assert_eq!(comment_window(false, "2026-08-02T02:37:37Z"), None);
        assert_eq!(
            comment_window(true, "2026-08-02T02:37:37Z"),
            Some("2026-08-02T02:37:37Z")
        );
    }

    /// With REVIEW_ON_UPDATE=false the webhook path ignores pushes, so
    /// reconciliation must too — otherwise a restart silently re-enables a mode
    /// the operator turned off, once per PR per boot.
    #[test]
    fn respects_review_on_update_off() {
        // Reviewed, then pushed to: the bot would not re-review, so nor do we.
        assert!(!should_reconcile(1000, Some(999), false));
        // Never reviewed at all: that's a real loss, recover it.
        assert!(should_reconcile(1000, None, false));
    }

    /// With it on, head-newer-than-review is the right question again.
    #[test]
    fn follows_the_head_when_update_reviews_are_on() {
        assert!(should_reconcile(1000, Some(999), true));
        assert!(!should_reconcile(1000, Some(1001), true));
        assert!(should_reconcile(1000, None, true));
    }

    #[test]
    fn no_comment_means_unreviewed() {
        assert!(needs_review(1000, None));
    }

    /// The whole correctness argument: a review posted before the head commit
    /// landed is a review of something else.
    #[test]
    fn stale_review_does_not_count_as_coverage() {
        assert!(needs_review(1000, Some(999)));
        assert!(!needs_review(1000, Some(1001)));
    }

    /// Ties are treated as covered: double-reviewing costs money and posts
    /// noise, while a one-second-late review is a miss nobody notices.
    #[test]
    fn a_tie_counts_as_covered() {
        assert!(!needs_review(1000, Some(1000)));
    }

    #[test]
    fn lookback_excludes_old_heads() {
        assert!(within_lookback(1000, 1000, 60));
        assert!(within_lookback(1000, 1060, 60));
        assert!(!within_lookback(1000, 1061, 60));
    }

    /// The 404-vs-everything-else split is what keeps an outage from logging
    /// like a quiet day, so pin it: only a real 404 may be classified as "gone".
    #[test]
    fn only_404_counts_as_not_found() {
        let gone = anyhow::Error::new(HttpStatus(404)).context("404 Not Found: ...");
        assert!(is_not_found(&gone));
        for code in [401, 403, 429, 500, 502] {
            let e = anyhow::Error::new(HttpStatus(code)).context("boom");
            assert!(!is_not_found(&e), "{code} must stay loud");
        }
        // A failure with no status at all (DNS, TLS, timeout) is not a 404.
        assert!(!is_not_found(&anyhow::anyhow!("connection reset")));
    }

    #[test]
    fn parses_repo_targets() {
        assert_eq!(
            parse_target("owner/repo"),
            Some(Target {
                provider: "github",
                repo: "owner/repo".into()
            })
        );
        assert_eq!(
            parse_target(" bitbucket:ws/repo "),
            Some(Target {
                provider: "bitbucket",
                repo: "ws/repo".into()
            })
        );
        assert_eq!(
            parse_target("github:owner/repo").map(|t| t.provider),
            Some("github")
        );
    }

    /// A typo'd prefix must not fall back to GitHub — it would 404 every repo
    /// and read as "nothing to reconcile".
    #[test]
    fn rejects_bad_targets() {
        for s in [
            "bitbuckt:ws/repo",
            "owner",
            "owner/",
            "/repo",
            "a/b/c",
            "github:",
        ] {
            assert_eq!(parse_target(s), None, "{s}");
        }
    }

    /// Fields are overridden on a cloned config rather than via `set_var`, which
    /// would race every concurrent `env::var` read in this parallel test binary.
    #[test]
    fn bb_auth_encodes_credentials_as_basic_base64() {
        let mut c = Config::from_env();
        c.bitbucket_email = "user@example.test".into();
        c.bitbucket_token = "secret-token".into();
        let header = bb_auth(&c).expect("both credentials set");
        assert_eq!(header, "Basic dXNlckBleGFtcGxlLnRlc3Q6c2VjcmV0LXRva2Vu");
    }

    #[test]
    fn bb_auth_rejects_missing_credentials() {
        let mut c = Config::from_env();
        c.bitbucket_email = String::new();
        c.bitbucket_token = "secret-token".into();
        assert!(bb_auth(&c).is_err(), "blank email must not authenticate");

        c.bitbucket_email = "  ".into();
        assert!(bb_auth(&c).is_err(), "whitespace-only email is still blank");

        c.bitbucket_email = "user@example.test".into();
        c.bitbucket_token = String::new();
        assert!(bb_auth(&c).is_err(), "blank token must not authenticate");
    }

    /// The page-cap escape hatch: when we can't prove a PR is unreviewed, we
    /// must not review it. i64::MAX stands in for "assume covered".
    #[test]
    fn unprovable_coverage_is_treated_as_covered() {
        assert!(!needs_review(i64::MAX - 1, Some(i64::MAX)));
    }

    #[test]
    fn disabled_without_repos() {
        let rc = ReconcileConfig {
            repos: vec![],
            lookback_secs: 3600,
            max_prs: 5,
        };
        assert!(!rc.enabled());
        let rc = ReconcileConfig {
            repos: vec![Target {
                provider: "github",
                repo: "a/b".into(),
            }],
            lookback_secs: 3600,
            max_prs: 0,
        };
        assert!(!rc.enabled(), "a zero cap must disable, not mean unlimited");
    }
}

#[cfg(test)]
mod placeholder_coverage_tests {
    use super::*;

    fn cfg() -> Config {
        Config::from_env()
    }

    /// The gap this closes. A placeholder is our own comment and newer than the
    /// head, so by every signal the sweep used to look at, it read as coverage.
    #[test]
    fn a_placeholder_is_not_coverage() {
        let c = cfg();
        let placeholder = format!(
            "🤖 **Automated review**\n\n⏳ _Reviewing this PR… (this comment will \
             update shortly)_\n\n_{}_",
            c.comment_marker
        );
        assert!(
            is_bot_comment(&c, &placeholder),
            "precondition: it IS our comment, which is why the timestamp fooled us"
        );
        assert!(
            !counts_as_coverage(&c, &placeholder),
            "but it is a promise of a review, not a review"
        );
    }

    /// The failure notice that replaces a placeholder is equally not a review.
    #[test]
    fn a_failure_notice_is_not_coverage() {
        let c = cfg();
        let failed = format!(
            "🤖 **Automated review**\n\n⚠️ _This review failed…_\n\n{}\n\n_{}_",
            crate::review::REVIEW_FAILED_MARKER,
            c.comment_marker
        );
        assert!(!counts_as_coverage(&c, &failed));
    }

    /// The other direction, and it matters as much: a completed review misread as
    /// incomplete would re-review every PR on every boot — duplicate comments and
    /// duplicate spend, forever.
    #[test]
    fn a_completed_review_is_coverage() {
        let c = cfg();
        let real = format!(
            "🤖 **Automated review**\n\nLooks fine.\n\n**Recommendation:** APPROVE\n\n_{}_",
            c.comment_marker
        );
        assert!(counts_as_coverage(&c, &real), "{real}");
    }

    /// Somebody else's comment is not coverage either, placeholder-looking or not.
    #[test]
    fn a_third_party_comment_is_not_coverage() {
        let c = cfg();
        assert!(!counts_as_coverage(&c, "LGTM, merging"));
    }
}
