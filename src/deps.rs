//! Dependency vulnerability scan (T3.8) via the [OSV.dev](https://osv.dev) API.
//!
//! Parses the dependency versions *added* by a PR from any changed lockfiles,
//! queries OSV.dev for known vulnerabilities, and renders a markdown advisory
//! block that [`crate::review`] appends to the summary comment.
//!
//! This is intentionally HTTP-only — no local resolver, no embeddings. It reads
//! the diff, so it only flags packages the PR actually introduces or bumps, and
//! it runs on the *raw* diff (lockfiles are dropped by the review's glob filter
//! before the LLM ever sees them). Fully fail-open: any parse or network error
//! yields zero advisories rather than failing the review.

use std::collections::BTreeSet;

use reqwest::Client;
use serde::Deserialize;

use crate::config::Config;
use crate::diff::split_diff_sections;

/// A resolved package coordinate to check against OSV.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageQuery {
    /// OSV ecosystem name (e.g. `crates.io`, `npm`, `PyPI`, `Go`).
    pub ecosystem: String,
    pub name: String,
    pub version: String,
}

/// One vulnerability advisory OSV reported for an added dependency.
#[derive(Debug, Clone)]
pub struct DepAdvisory {
    pub ecosystem: String,
    pub package: String,
    pub version: String,
    /// OSV / GHSA / CVE id (e.g. `RUSTSEC-2021-0079`, `GHSA-xxxx`).
    pub id: String,
    pub summary: String,
    /// Coarse severity label (`CRITICAL`/`HIGH`/`MEDIUM`/`LOW`) when OSV reports one.
    pub severity: Option<String>,
    /// First fixed version for this package, when known.
    pub fixed: Option<String>,
    /// Human-facing advisory URL.
    pub url: String,
}

/// Scan the raw PR diff for vulnerable dependencies added by the PR.
///
/// Returns an empty vector when the scan is disabled, no lockfiles changed, or
/// anything goes wrong (fail-open). Callers should treat the result as advisory
/// context to append to the review summary.
///
/// # Examples
/// ```no_run
/// # async fn demo(client: &reqwest::Client, cfg: &pr_review_core::config::Config, diff: &str) {
/// let advisories = pr_review_core::deps::scan(client, cfg, diff).await;
/// for a in &advisories {
///     println!("{} {}@{}: {}", a.ecosystem, a.package, a.version, a.id);
/// }
/// # }
/// ```
pub async fn scan(client: &Client, cfg: &Config, diff: &str) -> Vec<DepAdvisory> {
    if !cfg.cve_scan {
        return Vec::new();
    }
    let mut pkgs = changed_packages(diff);
    // Stable order + dedupe so a package pinned in two lockfiles is queried once.
    pkgs.sort();
    pkgs.dedup();
    if pkgs.is_empty() {
        return Vec::new();
    }
    if pkgs.len() > cfg.cve_max_packages {
        tracing::info!(
            "OSV scan: {} changed packages exceed CVE_MAX_PACKAGES={}, checking the first {}",
            pkgs.len(),
            cfg.cve_max_packages,
            cfg.cve_max_packages
        );
        pkgs.truncate(cfg.cve_max_packages);
    }
    match query_osv(client, cfg, &pkgs).await {
        Ok(mut advisories) => {
            advisories.sort_by(|a, b| {
                severity_rank(b.severity.as_deref())
                    .cmp(&severity_rank(a.severity.as_deref()))
                    .then(a.package.cmp(&b.package))
            });
            advisories
        }
        Err(e) => {
            tracing::warn!("OSV scan failed ({e:#}); skipping dependency advisories");
            Vec::new()
        }
    }
}

/// Coarse rank for sorting advisories (higher = more urgent).
fn severity_rank(sev: Option<&str>) -> u8 {
    match sev.map(|s| s.to_ascii_uppercase()).as_deref() {
        Some("CRITICAL") => 4,
        Some("HIGH") => 3,
        Some("MODERATE") | Some("MEDIUM") => 2,
        Some("LOW") => 1,
        _ => 0,
    }
}

/// Render the advisory block appended to the review summary. Empty input yields
/// an empty string so callers can unconditionally push the result.
///
/// # Examples
/// ```
/// # use pr_review_core::deps::{render_advisories, DepAdvisory};
/// let advs = vec![DepAdvisory {
///     ecosystem: "crates.io".into(), package: "foo".into(), version: "1.0.0".into(),
///     id: "RUSTSEC-2020-0001".into(), summary: "bad thing".into(),
///     severity: Some("HIGH".into()), fixed: Some("1.0.1".into()),
///     url: "https://osv.dev/RUSTSEC-2020-0001".into(),
/// }];
/// let md = render_advisories(&advs);
/// assert!(md.contains("Dependency advisories"));
/// assert!(md.contains("foo@1.0.0"));
/// ```
pub fn render_advisories(advisories: &[DepAdvisory]) -> String {
    if advisories.is_empty() {
        return String::new();
    }
    let n = advisories.len();
    let noun = if n == 1 { "advisory" } else { "advisories" };
    let mut s = format!(
        "## 🔒 Dependency advisories\n\n{n} known {noun} in dependencies added or bumped by this PR:\n"
    );
    for a in advisories {
        let sev = a
            .severity
            .as_deref()
            .map(|s| format!(" ({})", s.to_ascii_uppercase()))
            .unwrap_or_default();
        let fix = match &a.fixed {
            Some(v) => format!(" Fixed in `{v}`."),
            None => " No fixed version listed.".to_string(),
        };
        let summary = a.summary.trim();
        let summary = if summary.is_empty() {
            String::new()
        } else if summary.ends_with(['.', '!', '?']) {
            format!(" — {summary}")
        } else {
            // Punctuate so it doesn't run into the trailing "Fixed in …".
            format!(" — {summary}.")
        };
        s.push_str(&format!(
            "\n- **{}** `{}@{}` — [{}]({}){sev}{summary}{fix}",
            a.ecosystem, a.package, a.version, a.id, a.url,
        ));
    }
    s
}

/// Extract the `(name, version)` coordinates *added* by the diff from every
/// changed lockfile it recognizes. Only added lines (`+`, excluding the `+++`
/// header) are considered, so an unchanged pin is never re-flagged.
///
/// Recognized files: `Cargo.lock`, `package-lock.json`, `yarn.lock`,
/// `pnpm-lock.yaml`, `go.sum`, `requirements.txt`, `Gemfile.lock`,
/// `composer.lock`.
///
/// # Examples
/// ```
/// # use pr_review_core::deps::changed_packages;
/// let diff = "diff --git a/Cargo.lock b/Cargo.lock\n+++ b/Cargo.lock\n@@ -1 +1 @@\n+[[package]]\n+name = \"foo\"\n+version = \"1.2.3\"\n";
/// let pkgs = changed_packages(diff);
/// assert_eq!(pkgs.len(), 1);
/// assert_eq!(pkgs[0].name, "foo");
/// assert_eq!(pkgs[0].version, "1.2.3");
/// assert_eq!(pkgs[0].ecosystem, "crates.io");
/// ```
pub fn changed_packages(diff: &str) -> Vec<PackageQuery> {
    let mut out: Vec<PackageQuery> = Vec::new();
    for (path, section) in split_diff_sections(diff) {
        let base = path
            .rsplit('/')
            .next()
            .unwrap_or(&path)
            .to_ascii_lowercase();
        // Collect the added lines (payload without the leading '+').
        let added: Vec<&str> = section
            .lines()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .map(|l| &l[1..])
            .collect();
        if added.is_empty() {
            continue;
        }
        let parsed: Vec<(String, String)> = match base.as_str() {
            "cargo.lock" => parse_cargo_lock(&added),
            "package-lock.json" => parse_package_lock(&added),
            "yarn.lock" => parse_yarn_lock(&added),
            "pnpm-lock.yaml" => parse_pnpm_lock(&added),
            "go.sum" => parse_go_sum(&added),
            "requirements.txt" => parse_requirements_txt(&added),
            "gemfile.lock" => parse_gemfile_lock(&added),
            "composer.lock" => parse_composer_lock(&added),
            _ => continue,
        };
        let ecosystem = ecosystem_for(&base);
        for (name, version) in parsed {
            if name.is_empty() || version.is_empty() {
                continue;
            }
            out.push(PackageQuery {
                ecosystem: ecosystem.to_string(),
                name,
                version,
            });
        }
    }
    out
}

/// Map a lockfile basename to its OSV ecosystem string.
fn ecosystem_for(base: &str) -> &'static str {
    match base {
        "cargo.lock" => "crates.io",
        "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml" => "npm",
        "go.sum" => "Go",
        "requirements.txt" => "PyPI",
        "gemfile.lock" => "RubyGems",
        "composer.lock" => "Packagist",
        _ => "",
    }
}

/// Strip surrounding double quotes if present.
fn unquote(s: &str) -> &str {
    s.trim().trim_matches('"')
}

/// True for a bare, OSV-queryable version (starts with a digit, no range
/// operators or whitespace). Guards against emitting `>= 1.0` style specs.
fn is_concrete_version(v: &str) -> bool {
    let v = v.trim();
    !v.is_empty()
        && v.chars().next().is_some_and(|c| c.is_ascii_digit())
        && !v.contains(|c: char| {
            c.is_whitespace() || matches!(c, '>' | '<' | '=' | '~' | '^' | '*' | '|')
        })
}

// ── per-ecosystem lockfile parsers (added lines only) ───────────────────────

/// `Cargo.lock`: paired `name = "x"` / `version = "y"` TOML lines.
fn parse_cargo_lock(added: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut pending: Option<String> = None;
    for line in added {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("name = ") {
            pending = Some(unquote(rest).to_string());
        } else if let Some(rest) = t.strip_prefix("version = ") {
            if let Some(name) = pending.take() {
                let v = unquote(rest);
                if is_concrete_version(v) {
                    out.push((name, v.to_string()));
                }
            }
        }
    }
    out
}

/// `package-lock.json` (v2/v3): `"node_modules/<name>": {` then `"version": "x"`.
fn parse_package_lock(added: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut pending: Option<String> = None;
    for line in added {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("\"node_modules/") {
            // Key looks like `"node_modules/<name>": {`. A nested path keeps only
            // the final package (after the last `node_modules/`).
            if let Some(end) = rest.find("\":") {
                let full = &rest[..end];
                let name = full.rsplit("node_modules/").next().unwrap_or(full);
                pending = Some(name.to_string());
            }
        } else if let Some(rest) = t.strip_prefix("\"version\":") {
            if let Some(name) = pending.take() {
                let v = unquote(rest.trim_end_matches(','));
                if is_concrete_version(v) {
                    out.push((name, v.to_string()));
                }
            }
        }
    }
    out
}

/// `yarn.lock` (classic + berry): a non-indented `"pkg@range", ...:` header, then
/// an indented `version "x"` (classic) or `version: x` (berry).
fn parse_yarn_lock(added: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut pending: Option<String> = None;
    for line in added {
        let is_indented = line.starts_with(' ') || line.starts_with('\t');
        let t = line.trim();
        if !is_indented && t.ends_with(':') && t.contains('@') {
            // Header: take the first descriptor, e.g. `"@scope/pkg@npm:^1.0.0":`.
            let first = t.trim_end_matches(':').split(',').next().unwrap_or("");
            let desc = unquote(first);
            // Split at the LAST '@' that isn't the leading scope '@'.
            let at = desc
                .char_indices()
                .skip(1)
                .find(|&(_, c)| c == '@')
                .map(|(i, _)| i);
            if let Some(i) = at {
                pending = Some(desc[..i].to_string());
            }
        } else if is_indented {
            let v = if let Some(rest) = t.strip_prefix("version ") {
                Some(unquote(rest).to_string())
            } else {
                t.strip_prefix("version:")
                    .map(|rest| unquote(rest).to_string())
            };
            if let (Some(name), Some(v)) = (pending.clone(), v) {
                if is_concrete_version(&v) {
                    out.push((name, v));
                    pending = None;
                }
            }
        }
    }
    out
}

/// `pnpm-lock.yaml`: package keys like `/@scope/pkg@1.2.3:` or `pkg@1.2.3:`.
fn parse_pnpm_lock(added: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in added {
        let t = line.trim();
        if !t.ends_with(':') {
            continue;
        }
        let key = t.trim_end_matches(':').trim_start_matches('/');
        let key = unquote(key);
        // Only a `name@version` key (ignore section headers like `packages:`).
        // Cut any peer-suffix in parentheses: `pkg@1.2.3(react@18.0.0)`.
        let core = key.split('(').next().unwrap_or(key);
        // Split at the last '@' after the (optional) leading scope '@'.
        let at = core
            .char_indices()
            .skip(1)
            .filter(|&(_, c)| c == '@')
            .last()
            .map(|(i, _)| i);
        if let Some(i) = at {
            let name = &core[..i];
            let version = &core[i + 1..];
            if !name.is_empty() && is_concrete_version(version) {
                out.push((name.to_string(), version.to_string()));
            }
        }
    }
    out
}

/// `go.sum`: `<module> <version>[/go.mod] <hash>`; version's leading `v` stripped.
fn parse_go_sum(added: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in added {
        let mut parts = line.split_whitespace();
        let (Some(module), Some(ver)) = (parts.next(), parts.next()) else {
            continue;
        };
        let ver = ver.trim_end_matches("/go.mod");
        let version = ver.strip_prefix('v').unwrap_or(ver);
        if is_concrete_version(version) {
            out.push((module.to_string(), version.to_string()));
        }
    }
    out
}

/// `requirements.txt`: `name[extra]==1.2.3` (only exact `==` pins).
fn parse_requirements_txt(added: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in added {
        let t = line.trim();
        if t.starts_with('#') || !t.contains("==") {
            continue;
        }
        let Some((left, right)) = t.split_once("==") else {
            continue;
        };
        // Drop extras: `name[extra]` -> `name`.
        let name = left.split('[').next().unwrap_or(left).trim();
        // Version ends at the first space, `;` marker, or comment.
        let version = right
            .split([' ', ';', '#', ','])
            .next()
            .unwrap_or("")
            .trim();
        if !name.is_empty() && is_concrete_version(version) {
            out.push((name.to_string(), version.to_string()));
        }
    }
    out
}

/// `Gemfile.lock`: `    name (1.2.3)` spec lines (bare version only).
fn parse_gemfile_lock(added: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in added {
        // Spec lines are indented; the version sits in parens with no operator.
        if !line.starts_with(' ') {
            continue;
        }
        let t = line.trim();
        let Some(open) = t.find(" (") else { continue };
        let name = t[..open].trim();
        let rest = &t[open + 2..];
        let Some(close) = rest.find(')') else {
            continue;
        };
        let version = rest[..close].trim();
        if !name.is_empty() && !name.contains(' ') && is_concrete_version(version) {
            out.push((name.to_string(), version.to_string()));
        }
    }
    out
}

/// `composer.lock`: paired `"name": "vendor/pkg"` / `"version": "1.2.3"`.
fn parse_composer_lock(added: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut pending: Option<String> = None;
    for line in added {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("\"name\":") {
            let name = unquote(rest.trim_end_matches(','));
            // Packagist names are `vendor/package`.
            if name.contains('/') {
                pending = Some(name.to_string());
            }
        } else if let Some(rest) = t.strip_prefix("\"version\":") {
            if let Some(name) = pending.take() {
                let mut v = unquote(rest.trim_end_matches(','));
                // Composer tags are often `v1.2.3`.
                v = v.strip_prefix('v').unwrap_or(v);
                if is_concrete_version(v) {
                    out.push((name, v.to_string()));
                }
            }
        }
    }
    out
}

// ── OSV.dev API ─────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct BatchQuery<'a> {
    version: &'a str,
    package: BatchPackage<'a>,
}
#[derive(serde::Serialize)]
struct BatchPackage<'a> {
    name: &'a str,
    ecosystem: &'a str,
}
#[derive(serde::Serialize)]
struct BatchReq<'a> {
    queries: Vec<BatchQuery<'a>>,
}

#[derive(Deserialize)]
struct BatchRes {
    #[serde(default)]
    results: Vec<BatchResult>,
}
#[derive(Deserialize)]
struct BatchResult {
    #[serde(default)]
    vulns: Vec<VulnRef>,
}
#[derive(Deserialize)]
struct VulnRef {
    id: String,
}

/// Query OSV's batch endpoint, then fetch details for the packages that have hits.
async fn query_osv(
    client: &Client,
    cfg: &Config,
    pkgs: &[PackageQuery],
) -> anyhow::Result<Vec<DepAdvisory>> {
    let queries = pkgs
        .iter()
        .map(|p| BatchQuery {
            version: &p.version,
            package: BatchPackage {
                name: &p.name,
                ecosystem: &p.ecosystem,
            },
        })
        .collect();
    let body = BatchReq { queries };

    let res = client
        .post(format!("{}/v1/querybatch", cfg.osv_api_base))
        .json(&body)
        .send()
        .await?;
    if !res.status().is_success() {
        anyhow::bail!("OSV querybatch {}", res.status());
    }
    let batch: BatchRes = res.json().await?;

    // Collect (package, vuln id) pairs. The batch response is aligned to `pkgs`.
    // Dedupe vuln details by id so a CVE hitting two packages is fetched once,
    // but keep every (package, id) pairing for rendering.
    let mut pairs: Vec<(&PackageQuery, String)> = Vec::new();
    let mut ids: BTreeSet<String> = BTreeSet::new();
    for (i, result) in batch.results.iter().enumerate() {
        let Some(pkg) = pkgs.get(i) else { continue };
        for v in &result.vulns {
            pairs.push((pkg, v.id.clone()));
            ids.insert(v.id.clone());
        }
    }
    if pairs.is_empty() {
        return Ok(Vec::new());
    }

    // Fetch details once per unique id (capped to bound fan-out).
    let mut details: std::collections::HashMap<String, VulnDetail> =
        std::collections::HashMap::new();
    for id in ids.iter().take(cfg.cve_max_packages) {
        if let Ok(Some(d)) = fetch_vuln(client, cfg, id).await {
            details.insert(id.clone(), d);
        }
    }

    let mut advisories = Vec::new();
    for (pkg, id) in pairs {
        let detail = details.get(&id);
        let (summary, severity, fixed) = match detail {
            Some(d) => (
                d.best_summary(),
                d.severity_label(),
                d.fixed_for(&pkg.ecosystem, &pkg.name),
            ),
            None => (String::new(), None, None),
        };
        advisories.push(DepAdvisory {
            ecosystem: pkg.ecosystem.clone(),
            package: pkg.name.clone(),
            version: pkg.version.clone(),
            url: format!("https://osv.dev/vulnerability/{id}"),
            id,
            summary,
            severity,
            fixed,
        });
    }
    Ok(advisories)
}

#[derive(Deserialize)]
struct VulnDetail {
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    details: Option<String>,
    #[serde(default)]
    database_specific: Option<serde_json::Value>,
    #[serde(default)]
    affected: Vec<Affected>,
}
#[derive(Deserialize)]
struct Affected {
    #[serde(default)]
    package: Option<AffectedPackage>,
    #[serde(default)]
    ranges: Vec<Range>,
    #[serde(default)]
    database_specific: Option<serde_json::Value>,
}
#[derive(Deserialize)]
struct AffectedPackage {
    #[serde(default)]
    ecosystem: Option<String>,
    #[serde(default)]
    name: Option<String>,
}
#[derive(Deserialize)]
struct Range {
    #[serde(default)]
    events: Vec<Event>,
}
#[derive(Deserialize)]
struct Event {
    #[serde(default)]
    fixed: Option<String>,
}

impl VulnDetail {
    /// One-line summary: prefer `summary`, else the first line of `details`.
    fn best_summary(&self) -> String {
        if let Some(s) = self.summary.as_deref() {
            if !s.trim().is_empty() {
                return s.trim().to_string();
            }
        }
        self.details
            .as_deref()
            .and_then(|d| d.lines().find(|l| !l.trim().is_empty()))
            .unwrap_or("")
            .trim()
            .to_string()
    }

    /// Coarse severity label pulled from `database_specific.severity` (top-level
    /// or per-affected — GHSA advisories carry `CRITICAL`/`HIGH`/…).
    fn severity_label(&self) -> Option<String> {
        let from = |v: &serde_json::Value| -> Option<String> {
            v.get("severity")
                .and_then(|s| s.as_str())
                .map(|s| s.to_ascii_uppercase())
        };
        if let Some(s) = self.database_specific.as_ref().and_then(from) {
            return Some(s);
        }
        self.affected
            .iter()
            .find_map(|a| a.database_specific.as_ref().and_then(from))
    }

    /// First `fixed` version for the affected entry matching `ecosystem`/`name`.
    fn fixed_for(&self, ecosystem: &str, name: &str) -> Option<String> {
        for a in &self.affected {
            let matches = a
                .package
                .as_ref()
                .map(|p| {
                    p.name.as_deref() == Some(name)
                        && p.ecosystem
                            .as_deref()
                            .is_some_and(|e| e.eq_ignore_ascii_case(ecosystem))
                })
                .unwrap_or(false);
            if !matches {
                continue;
            }
            for r in &a.ranges {
                if let Some(f) = r.events.iter().find_map(|e| e.fixed.clone()) {
                    return Some(f);
                }
            }
        }
        None
    }
}

/// Fetch one vulnerability's detail record. `Ok(None)` on a 404.
async fn fetch_vuln(client: &Client, cfg: &Config, id: &str) -> anyhow::Result<Option<VulnDetail>> {
    let res = client
        .get(format!("{}/v1/vulns/{id}", cfg.osv_api_base))
        .send()
        .await?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !res.status().is_success() {
        anyhow::bail!("OSV vulns/{id} {}", res.status());
    }
    Ok(Some(res.json().await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(path: &str, added: &[&str]) -> String {
        let mut s = format!("diff --git a/{path} b/{path}\n+++ b/{path}\n@@ -0,0 +1 @@\n");
        for l in added {
            s.push('+');
            s.push_str(l);
            s.push('\n');
        }
        s
    }

    #[test]
    fn cargo_lock_pairs_name_and_version() {
        let d = section(
            "Cargo.lock",
            &["[[package]]", "name = \"time\"", "version = \"0.1.44\""],
        );
        let pkgs = changed_packages(&d);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].ecosystem, "crates.io");
        assert_eq!(pkgs[0].name, "time");
        assert_eq!(pkgs[0].version, "0.1.44");
    }

    #[test]
    fn package_lock_v3_node_modules() {
        let d = section(
            "package-lock.json",
            &[
                "    \"node_modules/lodash\": {",
                "      \"version\": \"4.17.19\",",
                "      \"resolved\": \"https://...\",",
            ],
        );
        let pkgs = changed_packages(&d);
        assert_eq!(
            pkgs,
            vec![PackageQuery {
                ecosystem: "npm".into(),
                name: "lodash".into(),
                version: "4.17.19".into()
            }]
        );
    }

    #[test]
    fn package_lock_scoped_nested() {
        let d = section(
            "package-lock.json",
            &[
                "    \"node_modules/foo/node_modules/@babel/core\": {",
                "      \"version\": \"7.0.0\",",
            ],
        );
        let pkgs = changed_packages(&d);
        assert_eq!(pkgs[0].name, "@babel/core");
        assert_eq!(pkgs[0].version, "7.0.0");
    }

    #[test]
    fn yarn_lock_classic_and_scoped() {
        let d = section(
            "yarn.lock",
            &[
                "lodash@^4.17.0:",
                "  version \"4.17.19\"",
                "\"@babel/core@npm:^7.0.0\":",
                "  version \"7.1.2\"",
            ],
        );
        let pkgs = changed_packages(&d);
        assert!(pkgs.contains(&PackageQuery {
            ecosystem: "npm".into(),
            name: "lodash".into(),
            version: "4.17.19".into()
        }));
        assert!(pkgs.contains(&PackageQuery {
            ecosystem: "npm".into(),
            name: "@babel/core".into(),
            version: "7.1.2".into()
        }));
    }

    #[test]
    fn pnpm_lock_keys() {
        let d = section(
            "pnpm-lock.yaml",
            &[
                "  /lodash@4.17.19:",
                "  /@babel/core@7.0.0(react@18.0.0):",
                "  packages:",
            ],
        );
        let pkgs = changed_packages(&d);
        assert!(pkgs.contains(&PackageQuery {
            ecosystem: "npm".into(),
            name: "lodash".into(),
            version: "4.17.19".into()
        }));
        assert!(pkgs.contains(&PackageQuery {
            ecosystem: "npm".into(),
            name: "@babel/core".into(),
            version: "7.0.0".into()
        }));
        // `packages:` is not a name@version key.
        assert_eq!(pkgs.len(), 2);
    }

    #[test]
    fn go_sum_strips_v_and_gomod() {
        let d = section(
            "go.sum",
            &[
                "github.com/gin-gonic/gin v1.6.3 h1:abc=",
                "github.com/gin-gonic/gin v1.6.3/go.mod h1:def=",
            ],
        );
        let mut pkgs = changed_packages(&d);
        pkgs.dedup();
        assert_eq!(pkgs[0].ecosystem, "Go");
        assert_eq!(pkgs[0].name, "github.com/gin-gonic/gin");
        assert_eq!(pkgs[0].version, "1.6.3");
    }

    #[test]
    fn requirements_txt_exact_pins_only() {
        let d = section(
            "requirements.txt",
            &[
                "django==2.2.0",
                "requests>=2.0",
                "flask[async]==1.1.4 ; python_version >= '3.6'",
            ],
        );
        let pkgs = changed_packages(&d);
        assert!(pkgs.contains(&PackageQuery {
            ecosystem: "PyPI".into(),
            name: "django".into(),
            version: "2.2.0".into()
        }));
        assert!(pkgs.contains(&PackageQuery {
            ecosystem: "PyPI".into(),
            name: "flask".into(),
            version: "1.1.4".into()
        }));
        // `requests>=2.0` is not an exact pin.
        assert!(!pkgs.iter().any(|p| p.name == "requests"));
    }

    #[test]
    fn gemfile_lock_spec_lines() {
        let d = section(
            "Gemfile.lock",
            &["    nokogiri (1.10.4)", "    rails (>= 5.0)"],
        );
        let pkgs = changed_packages(&d);
        assert!(pkgs.contains(&PackageQuery {
            ecosystem: "RubyGems".into(),
            name: "nokogiri".into(),
            version: "1.10.4".into()
        }));
        // A dependency constraint (operator) is not a concrete pin.
        assert!(!pkgs.iter().any(|p| p.name == "rails"));
    }

    #[test]
    fn composer_lock_name_version_pairs() {
        let d = section(
            "composer.lock",
            &[
                "            \"name\": \"monolog/monolog\",",
                "            \"version\": \"1.11.0\",",
            ],
        );
        let pkgs = changed_packages(&d);
        assert_eq!(pkgs[0].ecosystem, "Packagist");
        assert_eq!(pkgs[0].name, "monolog/monolog");
        assert_eq!(pkgs[0].version, "1.11.0");
    }

    #[test]
    fn ignores_unrelated_files() {
        let d = section("src/main.rs", &["let x = 1;", "version = \"9.9.9\""]);
        assert!(changed_packages(&d).is_empty());
    }

    #[test]
    fn render_is_empty_for_no_advisories() {
        assert_eq!(render_advisories(&[]), "");
    }

    /// Live end-to-end scan against the real OSV.dev API. Ignored by default
    /// (needs network); run with `cargo test --lib deps -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "hits the live OSV.dev API"]
    async fn osv_scan_flags_known_vulnerable_crate() {
        let diff = section(
            "Cargo.lock",
            &["[[package]]", "name = \"time\"", "version = \"0.1.44\""],
        );
        let cfg = crate::config::Config::from_env();
        let client = reqwest::Client::new();
        let advisories = scan(&client, &cfg, &diff).await;
        assert!(
            advisories.iter().any(|a| a.id == "RUSTSEC-2020-0071"),
            "expected RUSTSEC-2020-0071 for time 0.1.44, got: {advisories:?}"
        );
        let a = advisories.iter().find(|a| a.package == "time").unwrap();
        assert_eq!(a.ecosystem, "crates.io");
        assert!(a.fixed.is_some(), "should report a fixed version");
        println!("{}", render_advisories(&advisories));
    }

    #[test]
    fn is_concrete_version_rejects_ranges() {
        assert!(is_concrete_version("1.2.3"));
        assert!(!is_concrete_version(">= 1.0"));
        assert!(!is_concrete_version("^1.0.0"));
        assert!(!is_concrete_version("latest"));
        assert!(!is_concrete_version(""));
    }
}
