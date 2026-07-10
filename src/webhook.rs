//! GitHub webhook helpers — signature verification and `pull_request` payload
//! parsing. Kept separate from the HTTP layer so it's easy to unit-test.

use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Verify GitHub's `X-Hub-Signature-256` header against the raw request body.
///
/// GitHub signs the body with HMAC-SHA256 keyed by the webhook secret and sends
/// `sha256=<hex>`. The comparison is constant-time (via `verify_slice`).
///
/// # Examples
/// ```
/// # use pr_review_core::webhook::verify_signature;
/// // wrong/absent signatures are rejected
/// assert!(!verify_signature("secret", b"{}", None));
/// assert!(!verify_signature("secret", b"{}", Some("sha256=deadbeef")));
/// ```
pub fn verify_signature(secret: &str, body: &[u8], signature_header: Option<&str>) -> bool {
    let Some(sig) = signature_header else {
        return false;
    };
    let Some(hex_sig) = sig.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = hex::decode(hex_sig) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

/// Verify GitLab's `X-Gitlab-Token` header. Unlike GitHub/Bitbucket, GitLab does
/// NOT HMAC-sign the body — it echoes the configured secret verbatim in the
/// header, so this is a plain (constant-time) equality check.
///
/// Returns `false` if the secret is empty or the header is missing.
///
/// # Examples
/// ```
/// # use pr_review_core::webhook::verify_gitlab_token;
/// assert!(verify_gitlab_token("s3cret", Some("s3cret")));
/// assert!(!verify_gitlab_token("s3cret", Some("nope")));
/// assert!(!verify_gitlab_token("s3cret", None));
/// assert!(!verify_gitlab_token("", Some("")));
/// ```
pub fn verify_gitlab_token(secret: &str, presented: Option<&str>) -> bool {
    if secret.is_empty() {
        return false;
    }
    let Some(presented) = presented else {
        return false;
    };
    // Constant-time-ish comparison: fold length + byte differences into one flag.
    let a = secret.as_bytes();
    let b = presented.as_bytes();
    let mut diff = (a.len() ^ b.len()) as u8;
    for (i, &byte) in a.iter().enumerate() {
        diff |= byte ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

/// The bits of a `pull_request` event we act on.
pub struct WebhookPr {
    pub repo: String,
    pub pr: u64,
    pub action: String,
}

/// Parse a GitLab "Merge Request Hook" payload. The MR is identified by its
/// `iid` (project-scoped), and the action lives under `object_attributes.action`
/// (`open`/`reopen`/`update`/`merge`/`close`). Mirrors [`WebhookPr`] so server
/// code can treat all providers uniformly.
pub fn parse_gitlab_mr_event(body: &[u8]) -> anyhow::Result<WebhookPr> {
    #[derive(Deserialize)]
    struct Project {
        path_with_namespace: String,
    }
    #[derive(Deserialize)]
    struct ObjectAttributes {
        iid: u64,
        action: Option<String>,
    }
    #[derive(Deserialize)]
    struct Event {
        project: Project,
        object_attributes: ObjectAttributes,
    }
    let ev: Event = serde_json::from_slice(body)?;
    Ok(WebhookPr {
        repo: ev.project.path_with_namespace,
        pr: ev.object_attributes.iid,
        action: ev.object_attributes.action.unwrap_or_default(),
    })
}

/// Which GitLab merge-request actions should trigger a review. `open`/`reopen`
/// always review; `update` (new commits pushed) only when `review_on_update` is
/// set. Mirrors GitHub's opened/synchronize split.
pub fn should_review_gitlab(action: &str, review_on_update: bool) -> bool {
    match action {
        "open" | "reopen" => true,
        "update" => review_on_update,
        _ => false,
    }
}

/// Parse a `pull_request` webhook payload.
pub fn parse_pull_request_event(body: &[u8]) -> anyhow::Result<WebhookPr> {
    #[derive(Deserialize)]
    struct Repo {
        full_name: String,
    }
    #[derive(Deserialize)]
    struct PullRequest {
        number: u64,
    }
    #[derive(Deserialize)]
    struct Event {
        action: String,
        repository: Repo,
        pull_request: PullRequest,
    }

    let ev: Event = serde_json::from_slice(body)?;
    Ok(WebhookPr {
        repo: ev.repository.full_name,
        pr: ev.pull_request.number,
        action: ev.action,
    })
}

/// Which GitHub `pull_request` actions should trigger a review. `opened`,
/// `reopened`, and `ready_for_review` (draft → ready) always review.
/// `synchronize` (new commits pushed) only reviews when `review_on_update` is
/// set — off by default so a burst of iterative pushes doesn't fire a fresh
/// (expensive) review per commit. Re-review on demand via `POST /review`.
pub fn should_review(action: &str, review_on_update: bool) -> bool {
    match action {
        "opened" | "reopened" | "ready_for_review" => true,
        "synchronize" => review_on_update,
        _ => false,
    }
}

/// Parse a Bitbucket pull-request webhook payload (repo full name + PR id).
/// The event type is carried in the `X-Event-Key` header, not the body.
pub fn parse_bitbucket_pr_event(body: &[u8]) -> anyhow::Result<(String, u64)> {
    #[derive(Deserialize)]
    struct Repo {
        full_name: String,
    }
    #[derive(Deserialize)]
    struct Pr {
        id: u64,
    }
    #[derive(Deserialize)]
    struct Event {
        repository: Repo,
        pullrequest: Pr,
    }
    let ev: Event = serde_json::from_slice(body)?;
    Ok((ev.repository.full_name, ev.pullrequest.id))
}

/// Which Bitbucket webhook event keys should trigger a review. `pullrequest:created`
/// always reviews; `pullrequest:updated` (new commits) only when `review_on_update`
/// is set. Mirrors GitHub's opened/synchronize split.
pub fn should_review_bitbucket(event_key: &str, review_on_update: bool) -> bool {
    match event_key {
        "pullrequest:created" => true,
        "pullrequest:updated" => review_on_update,
        _ => false,
    }
}

/// Parse a Bitbucket `pullrequest:comment_created` payload (the `/review` command
/// channel): repo full name, PR id, and the comment's raw text. Bitbucket nests
/// the body under `comment.content.raw`. The event type comes from the
/// `X-Event-Key` header, not the body.
pub fn parse_bitbucket_comment_event(body: &[u8]) -> anyhow::Result<(String, u64, String)> {
    #[derive(Deserialize)]
    struct Repo {
        full_name: String,
    }
    #[derive(Deserialize)]
    struct Pr {
        id: u64,
    }
    #[derive(Deserialize)]
    struct Content {
        raw: String,
    }
    #[derive(Deserialize)]
    struct Comment {
        content: Content,
    }
    #[derive(Deserialize)]
    struct Event {
        repository: Repo,
        pullrequest: Pr,
        comment: Comment,
    }
    let ev: Event = serde_json::from_slice(body)?;
    Ok((
        ev.repository.full_name,
        ev.pullrequest.id,
        ev.comment.content.raw,
    ))
}

/// The bits of an `issue_comment` event we act on.
pub struct WebhookComment {
    pub repo: String,
    pub pr: u64,
    pub action: String,
    pub body: String,
    /// `issue_comment` fires for both issues and PRs; true only for a PR
    /// (GitHub includes an `issue.pull_request` object in that case).
    pub is_pull_request: bool,
}

/// Parse an `issue_comment` webhook payload (the `/review` command channel).
pub fn parse_issue_comment_event(body: &[u8]) -> anyhow::Result<WebhookComment> {
    #[derive(Deserialize)]
    struct Repo {
        full_name: String,
    }
    #[derive(Deserialize)]
    struct Issue {
        number: u64,
        #[serde(default)]
        pull_request: Option<serde_json::Value>,
    }
    #[derive(Deserialize)]
    struct Comment {
        body: String,
    }
    #[derive(Deserialize)]
    struct Event {
        action: String,
        issue: Issue,
        comment: Comment,
        repository: Repo,
    }
    let ev: Event = serde_json::from_slice(body)?;
    Ok(WebhookComment {
        repo: ev.repository.full_name,
        pr: ev.issue.number,
        action: ev.action,
        body: ev.comment.body,
        is_pull_request: ev.issue.pull_request.is_some(),
    })
}

/// Whether an `issue_comment` should trigger a review: a newly-created comment
/// on a PR whose body is exactly the `/review` command (surrounding whitespace
/// ignored). Editing or deleting a comment, or a comment on a plain issue, does
/// not trigger.
pub fn is_review_command(action: &str, is_pull_request: bool, body: &str) -> bool {
    action == "created" && is_pull_request && body.trim() == "/review"
}

#[cfg(test)]
mod tests {
    use super::{
        is_review_command, parse_bitbucket_comment_event, parse_gitlab_mr_event, should_review,
        should_review_bitbucket, should_review_gitlab, verify_gitlab_token,
    };

    #[test]
    fn opened_reopened_ready_always_review() {
        for action in ["opened", "reopened", "ready_for_review"] {
            assert!(should_review(action, false), "{action} should review");
            assert!(should_review(action, true), "{action} should review");
        }
    }

    #[test]
    fn synchronize_is_gated_by_flag() {
        assert!(!should_review("synchronize", false));
        assert!(should_review("synchronize", true));
    }

    #[test]
    fn unknown_github_action_ignored() {
        assert!(!should_review("labeled", false));
        assert!(!should_review("labeled", true));
    }

    #[test]
    fn bitbucket_updated_is_gated_by_flag() {
        assert!(should_review_bitbucket("pullrequest:created", false));
        assert!(!should_review_bitbucket("pullrequest:updated", false));
        assert!(should_review_bitbucket("pullrequest:updated", true));
        assert!(!should_review_bitbucket("pullrequest:fulfilled", false));
    }

    #[test]
    fn review_command_only_on_created_pr_comment() {
        assert!(is_review_command("created", true, "/review"));
        assert!(is_review_command("created", true, "  /review\n")); // whitespace ok
        assert!(!is_review_command("edited", true, "/review")); // not a new comment
        assert!(!is_review_command("created", false, "/review")); // plain issue, not a PR
        assert!(!is_review_command("created", true, "please /review")); // not the command
        assert!(!is_review_command("created", true, "/reviews")); // no fuzzy match
    }

    #[test]
    fn gitlab_token_matches_only_on_exact_secret() {
        assert!(verify_gitlab_token("s3cret", Some("s3cret")));
        assert!(!verify_gitlab_token("s3cret", Some("s3cre"))); // shorter
        assert!(!verify_gitlab_token("s3cret", Some("s3cretx"))); // longer
        assert!(!verify_gitlab_token("s3cret", Some("nope")));
        assert!(!verify_gitlab_token("s3cret", None)); // missing header
        assert!(!verify_gitlab_token("", Some(""))); // empty secret never verifies
        assert!(!verify_gitlab_token("", Some("anything")));
    }

    #[test]
    fn gitlab_update_is_gated_by_flag() {
        for action in ["open", "reopen"] {
            assert!(
                should_review_gitlab(action, false),
                "{action} should review"
            );
            assert!(should_review_gitlab(action, true), "{action} should review");
        }
        assert!(!should_review_gitlab("update", false));
        assert!(should_review_gitlab("update", true));
        assert!(!should_review_gitlab("merge", false));
        assert!(!should_review_gitlab("close", true));
    }

    #[test]
    fn parse_gitlab_mr_extracts_repo_iid_and_action() {
        let body = br#"{
            "object_kind": "merge_request",
            "project": { "path_with_namespace": "group/sub/project" },
            "object_attributes": { "iid": 7, "action": "open" }
        }"#;
        let ev = parse_gitlab_mr_event(body).unwrap();
        assert_eq!(ev.repo, "group/sub/project");
        assert_eq!(ev.pr, 7);
        assert_eq!(ev.action, "open");
        assert!(should_review_gitlab(&ev.action, false));
    }

    #[test]
    fn parse_bitbucket_comment_extracts_repo_pr_and_body() {
        let body = br#"{
            "repository": { "full_name": "ws/repo" },
            "pullrequest": { "id": 42 },
            "comment": { "content": { "raw": "/review" } }
        }"#;
        let (repo, pr, text) = parse_bitbucket_comment_event(body).unwrap();
        assert_eq!(repo, "ws/repo");
        assert_eq!(pr, 42);
        assert_eq!(text, "/review");
        // The parsed body feeds the same provider-neutral command check as GitHub.
        assert!(is_review_command("created", true, &text));
    }
}
