//! Credential redaction for text about to leave the process.
//!
//! A bot's error paths forward one string to several places at once — a PR
//! comment (on a repo that may be public), an alert channel, `tracing`, a status
//! page. The engine reports a failed git command with its whole argv, and the
//! clone URL in that argv carries live auth:
//! `https://x-access-token:ghs_…@github.com/owner/repo.git`, or a Bitbucket API
//! token, which is long-lived and stays valid until someone rotates it. A clip
//! to a few hundred characters does not help: the credential sits well inside
//! the window.
//!
//! Both bots built on this crate carried the same function, one with two more
//! token prefixes than the other. This is the union, so a prefix added for one
//! consumer protects every consumer.

/// Token prefixes worth catching outside a URL. GitHub: installation (`ghs_`),
/// personal (`ghp_`), OAuth (`gho_`), user-to-server (`ghu_`), refresh (`ghr_`),
/// fine-grained PAT. Atlassian: scoped (`ATATT`) and container (`ATCTT`) tokens.
///
/// Deliberately not covered: the pre-2021 classic GitHub PAT, 40 hex chars with
/// no prefix. It is indistinguishable from a git SHA, and error text is full of
/// those — redacting the shape would blind every error that names a commit, to
/// guard a format GitHub stopped issuing. Such a token in a clone URL is still
/// caught by the userinfo pass; only a bare one would slip.
const TOKEN_PREFIXES: [&str; 8] = [
    "ghs_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghr_",
    "github_pat_",
    "ATATT",
    "ATCTT",
];

/// Shortest colon-less URL userinfo treated as a credential rather than a
/// username. GitHub and Atlassian tokens are 36+ characters; usernames are
/// rarely past 20.
const USERINFO_TOKEN_MIN: usize = 20;

/// Strip credentials out of `s`.
///
/// Two passes, because either alone leaves a hole: URL userinfo catches any
/// scheme-embedded password whatever its shape, and the prefix scan catches a
/// bare token quoted somewhere no URL parse would reach. A token's prefix is
/// kept, since which *kind* of credential leaked is the first thing a reader
/// needs and is not itself the secret.
///
/// ```
/// use pr_review_core::redact::redact_secrets;
/// assert_eq!(
///     redact_secrets("git clone https://x-access-token:ghs_abc@github.com/o/r.git failed"),
///     "git clone https://x-access-token:***@github.com/o/r.git failed"
/// );
/// assert_eq!(redact_secrets("tried ghp_AAAA and ATATT9zz, both 401"), "tried ghp_*** and ATATT***, both 401");
/// ```
pub fn redact_secrets(s: &str) -> String {
    const REDACTED: &str = "***";
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        // `://user:PASSWORD@host` — bounded to the authority, so an unrelated
        // `@` later in the message (an email, a mention) can't drag the
        // redaction across half the error.
        if s[i..].starts_with("://") {
            let auth_start = i + 3;
            let auth_end = bytes[auth_start..]
                .iter()
                .position(|&c| {
                    matches!(
                        c,
                        b'@' | b'/' | b'?' | b'#' | b'"' | b'\'' | b' ' | b'\n' | b'\t'
                    )
                })
                .map(|p| auth_start + p);
            if let Some(end) = auth_end {
                if bytes[end] == b'@' {
                    if let Some(colon) = s[auth_start..end].find(':') {
                        out.push_str(&s[i..auth_start + colon + 1]);
                        out.push_str(REDACTED);
                        i = end;
                        continue;
                    }
                    // No `:`: the userinfo is either a username (`git@`,
                    // `user@`) or the credential itself, as in
                    // `https://<token>@github.com/…`, where an unprefixed token
                    // would slip past the prefix scan. Every token format this
                    // guards is far longer than a username, so length tells
                    // them apart and a username stays readable.
                    if end - auth_start >= USERINFO_TOKEN_MIN {
                        out.push_str(&s[i..auth_start]);
                        out.push_str(REDACTED);
                        i = end;
                        continue;
                    }
                }
            }
        }
        // A bare token: keep the prefix (it names what leaked, which is what a
        // reader of the log actually needs) and drop the secret body.
        // Only at the start of a word: `highs_and_lows` contains `ghs_` but is an
        // identifier, and redacting it would mangle the diagnosis.
        let word_start = i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if let Some(prefix) = TOKEN_PREFIXES
            .iter()
            .find(|p| word_start && s[i..].starts_with(**p))
        {
            out.push_str(prefix);
            out.push_str(REDACTED);
            i += prefix.len();
            while i < s.len()
                && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'-' | b'.'))
            {
                i += 1;
            }
            continue;
        }
        let ch = s[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::redact_secrets;

    #[test]
    fn a_github_clone_failure_cannot_publish_its_token() {
        let err = "clone repo for review: git clone failed: git [\"clone\", \
                   \"--depth\", \"1\", \"--quiet\", \
                   \"https://x-access-token:ghs_4355181_eyJhbGciOiJFUzI1NiJ9.abc-def.xyz@github.com/o/r.git\", \
                   \"/tmp/.tmpkyDFyz\"] failed: error: RPC failed";
        let safe = redact_secrets(err);
        assert!(!safe.contains("ghs_4355181"));
        assert!(!safe.contains("eyJhbGciOiJFUzI1NiJ9"));
        // Everything an operator needs to diagnose it survives.
        assert!(safe.contains("https://x-access-token:***@github.com/o/r.git"));
        assert!(safe.contains("RPC failed"));
    }

    /// The one that matters more: an Atlassian API token is long-lived, so a
    /// leaked one stays valid until somebody notices and rotates it.
    #[test]
    fn a_bitbucket_clone_failure_cannot_publish_its_token() {
        let err = "git clone failed: git [\"clone\", \
                   \"https://x-bitbucket-api-token-auth:ATATT3xFfGF0abc123@bitbucket.org/ws/r.git\"] \
                   failed: fatal: early EOF";
        let safe = redact_secrets(err);
        assert!(!safe.contains("ATATT3xFfGF0abc123"));
        assert!(safe.contains("https://x-bitbucket-api-token-auth:***@bitbucket.org/ws/r.git"));
        assert!(safe.contains("early EOF"));
    }

    #[test]
    fn bare_tokens_are_redacted_by_prefix_and_keep_the_prefix() {
        assert_eq!(
            redact_secrets("tried ghp_AAAA1111bbbb and github_pat_11ABC_xyz.def, both 401"),
            "tried ghp_*** and github_pat_***, both 401"
        );
        assert_eq!(
            redact_secrets("tried ghp_AAAA1111 and ATATT9zz-yy, both 401"),
            "tried ghp_*** and ATATT***, both 401"
        );
    }

    #[test]
    fn a_username_in_a_url_and_a_prefix_inside_a_word_are_left_alone() {
        for msg in [
            "fetch ssh://git@github.com/o/r.git failed",
            "fetch https://user@host.example/x failed",
            "highs_and_lows and rights_x are identifiers",
        ] {
            assert_eq!(redact_secrets(msg), msg);
        }
    }

    #[test]
    fn a_query_or_fragment_ends_the_authority() {
        let msg = "GET https://host.example?next=me@example.com failed";
        assert_eq!(redact_secrets(msg), msg);
    }

    #[test]
    fn a_token_used_as_the_whole_userinfo_is_redacted() {
        assert_eq!(
            redact_secrets(
                "clone https://0123456789abcdef0123456789abcdef01234567@github.com/o/r.git"
            ),
            "clone https://***@github.com/o/r.git"
        );
    }

    /// An unrelated `@` later in the message must not swallow the text between —
    /// that would redact the diagnosis away.
    #[test]
    fn redaction_stops_at_the_authority() {
        let msg = "cloning https://github.com/o/r.git failed; ask me@example.com";
        assert_eq!(redact_secrets(msg), msg);
    }

    #[test]
    fn redaction_is_utf8_safe() {
        assert_eq!(
            redact_secrets("réview ✅ done — ghs_secret"),
            "réview ✅ done — ghs_***"
        );
    }
}
