//! Outstanding findings on a pull request, as structured data.
//!
//! A review's findings live as provider comments once they are posted, and until
//! now the only code that read them back was reconciliation — which needs them to
//! decide what to resolve, and throws the rest away. A coding agent wants the
//! same question answered for a different reason: *what is still open on this PR,
//! and what did the reviewer actually say?*
//!
//! ## What "outstanding" means here
//!
//! Not "every comment containing the bot marker". A review comment has a
//! lifecycle — posted, matched again on a later round, resolved when the code
//! moved and the finding stopped recurring — and flattening that to "the bot said
//! this once" hands an agent a list of things that were fixed last week.
//!
//! [`FindingState`] is that lifecycle, taken from the semantics reconciliation
//! already uses ([`crate::providers`]'s fingerprint marker, and GitHub's resolved
//! flag), not invented here.
//!
//! ## Provider support is not uniform, and this module says so
//!
//! The three providers do genuinely different things, so a uniform API over them
//! would have to lie:
//!
//! - **GitHub** has resolvable review threads, and this crate writes a hidden
//!   fingerprint marker into each one. Full lifecycle.
//! - **GitLab** *deletes and reposts* the bot's inline discussions on every run
//!   (see `providers::gitlab`), so nothing on an MR survives a round to have a
//!   lifecycle. Every comment present is by construction from the latest review.
//! - **Bitbucket** renders HTML comments literally, so the fingerprint marker was
//!   never written there and reconciliation was never built.
//!
//! Rather than return an empty list for the two that cannot answer — which reads
//! as "no open findings", the most dangerous possible wrong answer — they return
//! [`FindingsOutcome::Unsupported`] naming what is missing.

use serde::Serialize;

use crate::config::Config;

/// Where a finding is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
pub enum FindingState {
    /// Posted by this bot, carrying a fingerprint, and not resolved. The set an
    /// agent should act on.
    Active,
    /// The thread was resolved — either by a human, or by a later review round
    /// that stopped flagging it once the code had moved.
    Resolved,
    /// Carries the bot marker but no fingerprint: a comment from before
    /// fingerprints existed, or one whose marker was edited. It cannot be matched
    /// to a finding, so it is reported separately rather than counted as open.
    Unparseable,
}

/// One finding as it currently exists on the pull request.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
pub struct OutstandingFinding {
    /// The reconciliation fingerprint — stable across rewordings of the same
    /// finding, and the identity to pass back to `resolve-findings`.
    ///
    /// `None` only for [`FindingState::Unparseable`].
    pub fingerprint: Option<String>,
    /// The provider's id for the comment, for a caller that wants to link to it.
    pub comment_id: String,
    /// The provider's thread id, where it has threads.
    pub thread_id: Option<String>,
    pub path: String,
    /// The line the provider currently tracks this thread at — which is not
    /// necessarily the line it was posted on, since a provider moves a thread as
    /// the file is edited beneath it.
    pub line: Option<u64>,
    /// The comment body as posted, rendered markdown and all.
    pub body: String,
    pub state: FindingState,
    /// The commit this finding was first written against.
    ///
    /// The revision question. A finding found on an old commit may already be
    /// fixed, and an agent that assumes otherwise edits code to fix something
    /// that is not there. Compare it against `head_sha` before acting — see
    /// [`FindingsOutput::revision_warning`].
    pub original_commit: Option<String>,
}

/// Whether the provider could answer at all.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", tag = "status")]
#[schemars(rename_all = "camelCase")]
pub enum FindingsOutcome {
    /// The provider tracks finding lifecycle and this is what it holds.
    Listed {
        /// Open findings — the ones worth acting on.
        active: Vec<OutstandingFinding>,
        /// Findings the provider considers closed. Returned rather than dropped
        /// so a caller can tell "resolved" from "never existed".
        resolved: Vec<OutstandingFinding>,
        /// Bot comments that carry no fingerprint and cannot be matched.
        unparseable: Vec<OutstandingFinding>,
    },
    /// This provider cannot answer the question, and why.
    ///
    /// Explicit rather than an empty `active` list. "No open findings" is a
    /// conclusion an agent acts on, and it must never be produced by a provider
    /// that simply was not asked.
    Unsupported { reason: String },
}

/// The findings currently on one pull request.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
pub struct FindingsOutput {
    pub provider: String,
    pub repo: String,
    pub pr: u64,
    /// The PR's head as of this call, when the provider reported one.
    pub head_sha: Option<String>,
    pub outcome: FindingsOutcome,
    /// Present when at least one active finding was written against a commit
    /// other than the current head.
    pub revision_warning: Option<String>,
}

impl FindingsOutput {
    /// Build the revision warning, if one is warranted.
    ///
    /// A finding's line number refers to the commit it was written against. The
    /// provider moves a thread as the file changes beneath it, which keeps the
    /// *thread* in the right place and says nothing about whether the finding
    /// still holds. An agent handed a line number with no provenance will happily
    /// edit code that was already fixed, so the mismatch is stated rather than
    /// left for the caller to notice.
    fn revision_warning(head: Option<&str>, active: &[OutstandingFinding]) -> Option<String> {
        let head = head?;
        let stale = active
            .iter()
            .filter(|f| f.original_commit.as_deref().is_some_and(|c| c != head))
            .count();
        (stale > 0).then(|| {
            format!(
                "{stale} of {} active finding(s) were written against an earlier commit than \
                 the current head ({head}). Their line numbers refer to that commit — re-read \
                 the code before acting on one, and do not assume it is still present.",
                active.len()
            )
        })
    }
}

/// List the findings currently on a pull request. Read-only: nothing is posted,
/// edited or resolved.
///
/// # Errors
/// If the provider name is unknown, or the provider call fails. A provider that
/// does not track lifecycle is **not** an error — it returns
/// [`FindingsOutcome::Unsupported`].
pub async fn get_findings(
    cfg: &Config,
    provider_name: &str,
    repo: &str,
    pr: u64,
) -> anyhow::Result<FindingsOutput> {
    use crate::providers::Provider;

    let provider = Provider::from_name(provider_name)?;
    let client = reqwest::Client::new();
    let meta = provider.get_meta(&client, cfg, repo, pr).await?;
    // The repository's own config decides the comment marker, which is what
    // "owned by this bot" means. Reading findings under the deployment's marker
    // when the repo set its own would silently return none.
    let effective = crate::review::load_repo_config(&provider, &client, cfg, repo, &meta).await;

    let outcome = provider
        .list_findings(&client, &effective, repo, pr, &meta)
        .await?;
    let revision_warning = match &outcome {
        FindingsOutcome::Listed { active, .. } => {
            FindingsOutput::revision_warning(meta.head_sha.as_deref(), active)
        }
        FindingsOutcome::Unsupported { .. } => None,
    };

    Ok(FindingsOutput {
        provider: provider.name().to_string(),
        repo: repo.to_string(),
        pr,
        head_sha: meta.head_sha.clone(),
        outcome,
        revision_warning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(fp: &str, commit: Option<&str>) -> OutstandingFinding {
        OutstandingFinding {
            fingerprint: Some(fp.to_string()),
            comment_id: "1".into(),
            thread_id: None,
            path: "src/a.rs".into(),
            line: Some(10),
            body: "something".into(),
            state: FindingState::Active,
            original_commit: commit.map(str::to_string),
        }
    }

    /// The revision question, which is the one an agent gets wrong silently: it
    /// reads a line number, edits that line, and the finding was about a commit
    /// two pushes ago.
    #[test]
    fn a_finding_from_an_earlier_commit_warns_about_its_revision() {
        let active = vec![finding("aa", Some("old")), finding("bb", Some("head"))];
        let warning = FindingsOutput::revision_warning(Some("head"), &active).expect("warns");
        assert!(warning.contains("1 of 2"), "{warning}");
        assert!(warning.contains("head"), "{warning}");
    }

    /// Everything on the current commit is not a warning — a warning nobody needs
    /// is one an agent learns to skip past.
    #[test]
    fn findings_on_the_current_head_do_not_warn() {
        let active = vec![finding("aa", Some("head"))];
        assert!(FindingsOutput::revision_warning(Some("head"), &active).is_none());
    }

    /// Unknown provenance must not be reported as a mismatch. A legacy thread
    /// carries no commit, and claiming it is stale would be inventing a fact.
    #[test]
    fn a_finding_of_unknown_provenance_does_not_warn() {
        let active = vec![finding("aa", None)];
        assert!(FindingsOutput::revision_warning(Some("head"), &active).is_none());
        // ...and with no head to compare against, there is nothing to say either.
        assert!(FindingsOutput::revision_warning(None, &[finding("aa", Some("old"))]).is_none());
    }
}

// ── handing findings to a coding agent ──────────────────────────────────────

/// What a caller should do with one selected finding.
///
/// These are **handoff states, not actions**. Kaniscope does not edit code, does
/// not resolve provider threads, and does not claim anything has been fixed. The
/// host agent owns every edit; this only says what kind of problem it is looking
/// at, so it can tell "go read this and decide" from "this needs a human".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
pub enum HandoffAction {
    /// Open, on the current head, with source context attached. Investigate it.
    Investigate,
    /// Open, but written against an earlier commit. The code may already have
    /// changed; re-read before acting.
    ReverifyAgainstHead,
    /// The provider already considers this closed. Nothing to do.
    AlreadyResolved,
    /// Asked for by identity, and no finding on the PR has that identity.
    NotFound,
    /// Carries the bot marker but no fingerprint, so it cannot be tied to a
    /// finding. A person should look at it.
    NeedsHumanJudgement,
}

/// One finding, packaged for a coding agent to act on.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
pub struct FindingHandoff {
    /// The identity that was asked for — echoed back so a caller can match up a
    /// request that produced [`HandoffAction::NotFound`].
    pub requested: String,
    pub action: HandoffAction,
    /// Absent only for [`HandoffAction::NotFound`].
    pub finding: Option<OutstandingFinding>,
    /// Why this action, in one sentence a caller can show a user.
    pub rationale: String,
}

/// A bundle of findings handed over for investigation.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
pub struct ResolveOutput {
    pub provider: String,
    pub repo: String,
    pub pr: u64,
    pub head_sha: Option<String>,
    pub handoffs: Vec<FindingHandoff>,
    /// Stated in the payload, not left to the caller's memory.
    ///
    /// This operation is named `resolve-findings` and it resolves nothing. An
    /// agent reading a field called "resolve" will assume something was done
    /// unless told otherwise, so it is told otherwise, in the data.
    pub disclaimer: String,
}

/// The sentence every [`ResolveOutput`] carries.
const NOTHING_WAS_CHANGED: &str = "Kaniscope did not modify any code, post anything, or resolve \
     any provider thread. These are findings selected for investigation; every edit and every \
     decision is the caller's.";

/// Select findings by fingerprint and package them for a coding agent.
///
/// **Deterministic.** No model call: this reads what `get-findings` read and
/// decides, from the lifecycle state and the revision, what kind of thing each
/// selected finding is. A second review pass hiding inside a "hand me the
/// findings" operation would be a surprise both in latency and in cost.
///
/// # Errors
/// If the provider call fails. A fingerprint that matches nothing is a
/// [`HandoffAction::NotFound`] handoff, not an error — asking about a finding
/// that has since been resolved is a normal thing for an agent to do.
pub async fn resolve_findings(
    cfg: &Config,
    provider_name: &str,
    repo: &str,
    pr: u64,
    fingerprints: &[String],
) -> anyhow::Result<ResolveOutput> {
    let listed = get_findings(cfg, provider_name, repo, pr).await?;
    let head = listed.head_sha.clone();

    let all: Vec<OutstandingFinding> = match &listed.outcome {
        FindingsOutcome::Listed {
            active,
            resolved,
            unparseable,
        } => active
            .iter()
            .chain(resolved)
            .chain(unparseable)
            .cloned()
            .collect(),
        FindingsOutcome::Unsupported { reason } => {
            anyhow::bail!("cannot select findings on this provider: {reason}")
        }
    };

    // Empty selection means "everything open", which is what an agent asking to
    // work through a PR's findings actually wants — and saves it round-tripping
    // a list it just received back through the same process.
    let wanted: Vec<String> = if fingerprints.is_empty() {
        all.iter()
            .filter(|f| f.state == FindingState::Active)
            .filter_map(|f| f.fingerprint.clone())
            .collect()
    } else {
        fingerprints.to_vec()
    };

    let handoffs = wanted
        .iter()
        .map(|fp| {
            let found = all
                .iter()
                .find(|f| f.fingerprint.as_deref() == Some(fp.as_str()));
            match found {
                None => FindingHandoff {
                    requested: fp.clone(),
                    action: HandoffAction::NotFound,
                    finding: None,
                    rationale: "No finding with that fingerprint is on this pull request. It may \
                                have been resolved and its thread deleted, or it may belong to a \
                                different PR."
                        .to_string(),
                },
                Some(f) => {
                    let (action, rationale) = classify(f, head.as_deref());
                    FindingHandoff {
                        requested: fp.clone(),
                        action,
                        finding: Some(f.clone()),
                        rationale,
                    }
                }
            }
        })
        .collect();

    Ok(ResolveOutput {
        provider: listed.provider,
        repo: listed.repo,
        pr: listed.pr,
        head_sha: head,
        handoffs,
        disclaimer: NOTHING_WAS_CHANGED.to_string(),
    })
}

/// Decide what kind of handoff one finding is, and say why.
///
/// Pure, so the decision table is directly testable — which matters because this
/// is the part an agent obeys.
fn classify(f: &OutstandingFinding, head: Option<&str>) -> (HandoffAction, String) {
    match f.state {
        FindingState::Resolved => (
            HandoffAction::AlreadyResolved,
            "This thread is already resolved on the pull request. Nothing to do unless you are \
             deliberately reopening it."
                .to_string(),
        ),
        FindingState::Unparseable => (
            HandoffAction::NeedsHumanJudgement,
            "This comment carries the bot marker but no fingerprint, so it cannot be matched to \
             a finding. A person should read it."
                .to_string(),
        ),
        FindingState::Active => match (f.original_commit.as_deref(), head) {
            // Known provenance, and it is not the current head: the code under it
            // may already have moved.
            (Some(orig), Some(h)) if orig != h => (
                HandoffAction::ReverifyAgainstHead,
                format!(
                    "Written against {orig}, but the head is now {h}. The line number refers to \
                     {orig} — read the current code before changing anything, and treat the \
                     finding as unconfirmed until you have."
                ),
            ),
            _ => (
                HandoffAction::Investigate,
                "Open on the current head. Read the cited code and decide whether the finding \
                 holds before changing anything."
                    .to_string(),
            ),
        },
    }
}

#[cfg(test)]
mod handoff_tests {
    use super::*;

    fn f(state: FindingState, commit: Option<&str>) -> OutstandingFinding {
        OutstandingFinding {
            fingerprint: Some("aa".into()),
            comment_id: "1".into(),
            thread_id: None,
            path: "src/a.rs".into(),
            line: Some(10),
            body: "something".into(),
            state,
            original_commit: commit.map(str::to_string),
        }
    }

    /// The states the handoff has to keep apart, and the one an agent would
    /// otherwise act on wrongly: a finding from an older commit reads exactly
    /// like a current one unless the revision is checked.
    #[test]
    fn each_lifecycle_state_maps_to_its_own_handoff() {
        assert_eq!(
            classify(&f(FindingState::Active, Some("head")), Some("head")).0,
            HandoffAction::Investigate
        );
        assert_eq!(
            classify(&f(FindingState::Active, Some("old")), Some("head")).0,
            HandoffAction::ReverifyAgainstHead
        );
        assert_eq!(
            classify(&f(FindingState::Resolved, Some("head")), Some("head")).0,
            HandoffAction::AlreadyResolved
        );
        assert_eq!(
            classify(&f(FindingState::Unparseable, None), Some("head")).0,
            HandoffAction::NeedsHumanJudgement
        );
    }

    /// Unknown provenance must not be reported as a revision mismatch — that
    /// would be asserting a fact this code does not have.
    #[test]
    fn unknown_provenance_is_investigated_not_flagged_as_stale() {
        assert_eq!(
            classify(&f(FindingState::Active, None), Some("head")).0,
            HandoffAction::Investigate
        );
    }

    /// The rationale names both commits, because "re-verify" without saying
    /// against what is an instruction an agent cannot follow.
    #[test]
    fn a_stale_findings_rationale_names_both_commits() {
        let (_, why) = classify(&f(FindingState::Active, Some("old")), Some("head"));
        assert!(why.contains("old") && why.contains("head"), "{why}");
    }

    /// The operation is called `resolve-findings` and resolves nothing. An agent
    /// reading that name will assume otherwise unless the payload says so.
    #[test]
    fn the_bundle_states_that_nothing_was_changed() {
        assert!(NOTHING_WAS_CHANGED.contains("did not modify any code"));
        assert!(NOTHING_WAS_CHANGED.contains("resolve"));
    }
}

// ── explaining one finding ──────────────────────────────────────────────────

/// A structured explanation of one finding, checked against real code.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
pub struct Explanation {
    /// What the finding asserts, restated plainly.
    pub claim: String,
    /// What in the code supports or contradicts it — file, line, and what is
    /// actually there.
    pub evidence: String,
    /// What goes wrong if the claim holds, in terms of behaviour rather than
    /// style. Empty when the explanation concludes the claim does not hold.
    pub affected_behavior: String,
    /// What the explanation could not settle. Never empty by convention: a
    /// second-hand reading of a diff always leaves something open, and an
    /// explanation that admits nothing is one nobody should trust.
    pub uncertainty: String,
    /// How a person could confirm or refute this — a test to write, a command to
    /// run, a line to read.
    pub suggested_verification: String,
    /// Whether the investigation found the finding to hold.
    pub verdict: ExplanationVerdict,
}

/// What the investigation concluded.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
pub enum ExplanationVerdict {
    /// The code supports the finding.
    Holds,
    /// The code contradicts it — a false positive.
    DoesNotHold,
    /// Not enough was readable to say. An honest answer, and the one a reviewer
    /// under pressure to look decisive will skip.
    Inconclusive,
}

/// One finding explained against a local checkout.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
pub struct ExplainOutput {
    /// The finding as given.
    pub finding: ExplainInput,
    pub explanation: Explanation,
    /// Set when the finding names a revision that is not the one that was read.
    ///
    /// The explanation is still returned — a mismatched revision usually still
    /// explains the finding usefully — but the caller is told the line numbers
    /// may not line up, rather than being left to assume they do.
    pub revision_warning: Option<String>,
}

/// The finding to explain. Deliberately not [`OutstandingFinding`]: a caller may
/// be explaining a finding it just received from a review and never posted.
#[derive(Debug, Clone, Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
pub struct ExplainInput {
    pub file: String,
    pub line: Option<u64>,
    /// What the reviewer said.
    pub body: String,
    pub severity: Option<String>,
    /// The commit the finding was written against, when known.
    pub original_commit: Option<String>,
}

/// The rubric for an explanation. Kept here rather than in each consumer for the
/// reason `injected_rules` exists: a prompt every backend is trusted to compose
/// is a prompt that drifts.
const EXPLAIN_SYSTEM_PROMPT: &str = r#"You are investigating ONE code-review finding against the real code, to decide whether it holds.

You are given the finding and the current contents of the file it names. Read the code. Decide.

Return ONLY a JSON object:
{"claim": "...", "evidence": "...", "affectedBehavior": "...", "uncertainty": "...", "suggestedVerification": "...", "verdict": "holds" | "doesNotHold" | "inconclusive"}

Rules:
- `evidence` cites what is ACTUALLY in the file — line numbers and the code at them. Do not describe code you were not shown.
- A finding you cannot check against the code you were given is `inconclusive`. It is not `holds` because the reviewer said so, and it is not `doesNotHold` because you could not find the problem. Guessing either way is worse than saying you could not tell.
- `doesNotHold` is a real and common answer. Reviewers produce false positives; confirming one because it was written confidently is the failure this pass exists to catch.
- `uncertainty` is never empty. Say what you could not see — other call sites, runtime values, the caller's intent.
- `affectedBehavior` describes what breaks, concretely. Leave it empty when the verdict is `doesNotHold`.
- Never propose an edit. You are explaining, not fixing."#;

/// Explain one finding against a local checkout.
///
/// Runs through the selected [`ReviewBackend`](crate::backend::ReviewBackend), so
/// a consumer on an agent CLI explains findings on the same backend it reviews
/// with — and the prompt is composed here, once, rather than in each consumer.
///
/// Never posts.
///
/// # Errors
/// If the file cannot be read, or the backend fails or returns unparseable JSON.
pub async fn explain_finding(
    cfg: &Config,
    backend: &dyn crate::backend::ReviewBackend,
    repo_root: &std::path::Path,
    finding: ExplainInput,
    head_sha: Option<&str>,
) -> anyhow::Result<ExplainOutput> {
    use anyhow::Context;

    // The same authorization the file review takes. An explanation reads a whole
    // file and returns its contents in prose, so a finding naming an excluded
    // path must not become a way around the filters.
    if !crate::filereview::path_is_reviewable(cfg, &finding.file) {
        anyhow::bail!(
            "`{}` is excluded by this repository's review file filters",
            finding.file
        );
    }
    let full = repo_root.join(&finding.file);
    let content =
        std::fs::read_to_string(&full).with_context(|| format!("reading {}", full.display()))?;

    let numbered: String = content
        .lines()
        .enumerate()
        .map(|(i, l)| format!("{}: {l}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let user =
        format!(
        "## The finding\nFile: {}\nLine: {}\nSeverity: {}\n\n{}\n\n## The file as it is now\n{}",
        finding.file,
        finding.line.map(|l| l.to_string()).unwrap_or_else(|| "(none)".into()),
        finding.severity.as_deref().unwrap_or("(none)"),
        finding.body.trim(),
        crate::clip(&numbered, cfg.max_diff_chars),
    );

    let raw = backend
        .complete(cfg, EXPLAIN_SYSTEM_PROMPT, &user)
        .await
        .context("explaining the finding")?;
    let explanation = parse_explanation(&raw)?;

    Ok(ExplainOutput {
        revision_warning: revision_mismatch(finding.original_commit.as_deref(), head_sha),
        finding,
        explanation,
    })
}

/// State a revision mismatch rather than implying the line numbers line up.
///
/// The handoff's rule: never promise that an old line number still maps to the
/// current checkout. Silence here would be that promise.
fn revision_mismatch(original: Option<&str>, head: Option<&str>) -> Option<String> {
    match (original, head) {
        (Some(o), Some(h)) if o != h => Some(format!(
            "This finding was written against {o}; the code read here is {h}. Its line number \
             refers to {o} and may not point at the same code now."
        )),
        // Unknown provenance says nothing, rather than guessing either way.
        _ => None,
    }
}

/// Parse the backend's JSON, tolerating the fences models wrap it in.
fn parse_explanation(raw: &str) -> anyhow::Result<Explanation> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Raw {
        claim: String,
        evidence: String,
        #[serde(default)]
        affected_behavior: String,
        #[serde(default)]
        uncertainty: String,
        #[serde(default)]
        suggested_verification: String,
        verdict: ExplanationVerdict,
    }

    // The same salvage the review path needs: models fence JSON in ```json blocks
    // and prepend a sentence, and discarding an otherwise-good answer over that
    // is a billed call thrown away.
    let text = raw.trim();
    let slice = match (text.find('{'), text.rfind('}')) {
        (Some(a), Some(b)) if b > a => &text[a..=b],
        _ => anyhow::bail!(
            "explanation response contained no JSON object: {}",
            crate::clip(text, 200)
        ),
    };
    let r: Raw = serde_json::from_str(slice)
        .with_context_msg(|| format!("parsing the explanation: {}", crate::clip(slice, 200)))?;

    Ok(Explanation {
        claim: r.claim,
        evidence: r.evidence,
        affected_behavior: r.affected_behavior,
        // An explanation that admits nothing is one nobody should trust, so the
        // field is filled rather than left blank when the model skips it.
        uncertainty: if r.uncertainty.trim().is_empty() {
            "The model did not state its uncertainty. Treat this explanation as less reliable \
             than one that did."
                .to_string()
        } else {
            r.uncertainty
        },
        suggested_verification: r.suggested_verification,
        verdict: r.verdict,
    })
}

/// `anyhow::Context` for a `serde_json::Result`, spelled locally so the parse
/// error keeps the offending text.
trait WithContextMsg<T> {
    fn with_context_msg<F: FnOnce() -> String>(self, f: F) -> anyhow::Result<T>;
}

impl<T> WithContextMsg<T> for Result<T, serde_json::Error> {
    fn with_context_msg<F: FnOnce() -> String>(self, f: F) -> anyhow::Result<T> {
        self.map_err(|e| anyhow::anyhow!("{}: {e}", f()))
    }
}

#[cfg(test)]
mod explain_tests {
    use super::*;

    /// Models fence JSON and prepend prose. Discarding an otherwise-good answer
    /// over that throws away a billed call.
    #[test]
    fn a_fenced_explanation_still_parses() {
        let raw = "Here's my analysis:\n```json\n{\"claim\":\"c\",\"evidence\":\"e\",\
                   \"affectedBehavior\":\"a\",\"uncertainty\":\"u\",\
                   \"suggestedVerification\":\"v\",\"verdict\":\"holds\"}\n```";
        let e = parse_explanation(raw).expect("parses");
        assert_eq!(e.verdict, ExplanationVerdict::Holds);
        assert_eq!(e.claim, "c");
    }

    /// An explanation with no stated uncertainty is less trustworthy, and says so
    /// rather than presenting an empty field as "nothing to worry about".
    #[test]
    fn a_missing_uncertainty_is_filled_in_not_left_blank() {
        let raw = r#"{"claim":"c","evidence":"e","verdict":"doesNotHold"}"#;
        let e = parse_explanation(raw).expect("parses");
        assert!(
            e.uncertainty.contains("did not state its uncertainty"),
            "{}",
            e.uncertainty
        );
        assert_eq!(e.verdict, ExplanationVerdict::DoesNotHold);
    }

    /// A response with no JSON at all is an error naming what came back — not a
    /// default-constructed explanation that reads like a real verdict.
    #[test]
    fn a_response_with_no_json_is_an_error() {
        let err = parse_explanation("I could not do that.").expect_err("must fail");
        assert!(err.to_string().contains("no JSON object"), "{err}");
    }

    /// The handoff's rule: never imply an old line number still maps to the
    /// current checkout.
    #[test]
    fn a_revision_mismatch_is_stated_explicitly() {
        let w = revision_mismatch(Some("old"), Some("head")).expect("warns");
        assert!(w.contains("old") && w.contains("head"), "{w}");
        // Matching revisions, and unknown ones, say nothing.
        assert!(revision_mismatch(Some("head"), Some("head")).is_none());
        assert!(revision_mismatch(None, Some("head")).is_none());
    }
}
