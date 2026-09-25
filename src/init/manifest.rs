//! What `crewd init` asks GitHub to create, and the one page that asks.
//!
//! [`PERMISSIONS`] is the single statement of what the App may do. The manifest is built from it
//! and the read-back in [`super::github`] compares against it, so a permission added to one and
//! not the other cannot pass as a match — which is the whole point of reading the App back rather
//! than trusting the form GitHub showed.

use serde_json::{Value, json};

/// Contents, issues and pull requests are written by the agent's tools and delivery; statuses and
/// checks are CI, which delivery only reads; metadata is implied by every other permission, and
/// GitHub reports it on the App whether or not the manifest names it, so it is named here.
pub const PERMISSIONS: &[(&str, &str)] = &[
    ("checks", "read"),
    ("contents", "write"),
    ("issues", "write"),
    ("metadata", "read"),
    ("pull_requests", "write"),
    ("statuses", "read"),
];

/// Where the created App says it comes from. Required by the manifest; shown on the App's page.
const HOMEPAGE: &str = "https://github.com/StGerman/crewd";

/// GitHub rejects an App name longer than this.
const MAX_NAME_LEN: usize = 34;

pub fn permissions() -> Value {
    PERMISSIONS.iter().map(|(k, v)| (k.to_string(), json!(v))).collect()
}

/// No webhook: the daemon polls, and a webhook URL pointing at a laptop would only fail.
pub fn manifest(name: &str, redirect_url: &str) -> Value {
    json!({
        "name": name,
        "url": HOMEPAGE,
        "redirect_url": redirect_url,
        "public": false,
        "hook_attributes": { "url": HOMEPAGE, "active": false },
        "default_permissions": permissions(),
        "default_events": [],
    })
}

/// `crew-<login>`, cut to GitHub's length limit. Without a login there is nothing unique to put
/// after `crew-`, so the caller's suffix — random, per run — stands in for it.
pub fn default_app_name(login: Option<&str>, fallback_suffix: &str) -> String {
    let who = login.map(str::trim).filter(|l| !l.is_empty()).unwrap_or(fallback_suffix);
    let mut name = format!("crew-{who}");
    name.truncate(MAX_NAME_LEN);
    name
}

/// GitHub's slug for an App name: lowercased, every run of other characters one `-`. Used only to
/// ask whether the name is already taken, so an approximation costs a missed pre-check, not a
/// wrong App — GitHub's own form still refuses a duplicate.
pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

/// The page the operator opens. It submits itself, so the first click is GitHub's own *Create*.
///
/// The manifest is sent by the browser, not by this process, because the Manifest flow is
/// defined as a form post from a page the user is looking at: that is what makes GitHub show the
/// confirmation with the permissions on it.
pub fn page(create_url: &str, manifest: &Value) -> String {
    let manifest = escape(&manifest.to_string());
    let action = escape(create_url);
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>crewd init</title></head>
<body onload="document.forms[0].submit()">
<form action="{action}" method="post">
<input type="hidden" name="manifest" value="{manifest}">
<p>Sending you to GitHub to create the App crewd will act as.</p>
<p>If GitHub says the name is taken, stop <code>crewd init</code> and run it again with
<code>--app-name &lt;another name&gt;</code>.</p>
<button type="submit">Continue to GitHub</button>
</form>
</body></html>
"#
    )
}

/// The page a finished callback shows while the operator's browser follows the redirect.
pub fn installing(install_url: &str) -> String {
    let url = escape(install_url);
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>crewd init</title></head>
<body><p>App created. <a href="{url}">Install it</a> on the repositories crewd should work on.</p>
</body></html>
"#
    )
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_name_is_crew_and_the_login_cut_to_githubs_limit() {
        assert_eq!(default_app_name(Some("StGerman"), "x"), "crew-StGerman");
        assert_eq!(default_app_name(None, "3f9a1c"), "crew-3f9a1c");
        assert_eq!(default_app_name(Some(" "), "3f9a1c"), "crew-3f9a1c");
        let long = "a".repeat(39);
        assert_eq!(default_app_name(Some(&long), "x").len(), MAX_NAME_LEN);
    }

    #[test]
    fn a_name_is_slugged_the_way_github_slugs_it() {
        assert_eq!(slugify("crew-StGerman"), "crew-stgerman");
        assert_eq!(slugify("My  Crew (dev)"), "my-crew-dev");
    }

    #[test]
    fn the_manifest_survives_being_embedded_in_the_form() {
        let m = manifest("a\"b<c>&'", "http://127.0.0.1:1/callback");
        let html = page("https://github.com/settings/apps/new?state=s", &m);
        let start = html.find("value=\"").unwrap() + 7;
        let end = start + html[start..].find('"').unwrap();
        let raw = html[start..end]
            .replace("&quot;", "\"")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&#39;", "'")
            .replace("&amp;", "&");
        assert_eq!(serde_json::from_str::<Value>(&raw).unwrap(), m);
    }
}
