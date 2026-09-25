//! The four GitHub calls `crewd init` makes, over the tracker's [`Http`] seam.
//!
//! The conversion answer is the one response in this codebase that carries a private key and a
//! client secret, so it is never held as text longer than the parse: [`Converted`] keeps the key
//! in a [`Pem`] whose `Debug` prints nothing, drops the client and webhook secrets by not having
//! fields for them, and no error built from a successful conversion quotes its body. That is what
//! makes "the key never reaches a log" a property of the types rather than of every future
//! `debug!` remembering not to.
//!
//! The App is signed for with its own key, not a user token, so the read-back proves what the
//! App *is* rather than what a user can see of it. The JWT here duplicates the signing in #64's
//! `credentials::GithubApp` because this branch was written before that one landed; whichever
//! merges second should keep one.

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use rustls_pki_types::PrivateKeyDer;
use rustls_pki_types::pem::PemObject;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::clock::Wall;
use crate::tracker::github::{Http, HttpResponse};

use super::InitError;
use super::manifest::PERMISSIONS;

pub const API_BASE: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";
/// GitHub rejects a JWT whose `iat` is in its future, so it is backdated past a skewed host clock.
const JWT_BACKDATE_S: i64 = 60;
/// GitHub's ceiling on a JWT's lifetime is ten minutes.
const JWT_LIFETIME_S: i64 = 9 * 60;

/// A private key in PEM form. Its `Debug` is what keeps it out of a `?`-formatted log field.
pub struct Pem(String);

impl Pem {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Pem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pem(<redacted>)")
    }
}

/// What the conversion returns that `init` keeps. The client id and secret and the webhook secret
/// are in the same response and deliberately have no field: nothing here uses them, and a secret
/// that is never deserialized cannot be logged.
#[derive(Debug)]
pub struct Converted {
    pub app_id: u64,
    pub slug: String,
    pub owner: String,
    pub pem: Pem,
}

fn headers(auth: Option<String>) -> Vec<(&'static str, String)> {
    let mut h = vec![
        ("Accept", "application/vnd.github+json".to_string()),
        ("X-GitHub-Api-Version", API_VERSION.to_string()),
        ("User-Agent", "crewd".to_string()),
    ];
    if let Some(token) = auth {
        h.push(("Authorization", format!("Bearer {token}")));
    }
    h
}

/// Whether an App already answers at `slug`. Only a public App is visible to an anonymous read,
/// so `false` means "not known to be taken" — GitHub's own form is the final word, and the init
/// page tells the operator what to do if it refuses.
pub fn name_taken(http: &dyn Http, slug: &str) -> Result<bool, InitError> {
    let url = format!("{API_BASE}/apps/{slug}");
    let resp = http.get(&url, &headers(None)).map_err(|e| InitError::Transport(e.0))?;
    tracing::debug!(status = resp.status, slug, "checked whether the app name is taken");
    Ok(resp.status == 200)
}

pub fn convert(http: &dyn Http, code: &str) -> Result<Converted, InitError> {
    let url = format!("{API_BASE}/app-manifests/{code}/conversions");
    let resp = http
        .send_json("POST", &url, &headers(None), b"")
        .map_err(|e| InitError::Conversion(format!("the request did not complete: {}", e.0)))?;
    // The status only: the body of a successful answer is the key.
    tracing::debug!(status = resp.status, "manifest conversion answered");
    if !(200..300).contains(&resp.status) {
        return Err(InitError::Conversion(format!("GitHub answered {}", snippet(&resp))));
    }

    #[derive(Deserialize)]
    struct Owner {
        login: String,
    }
    #[derive(Deserialize)]
    struct Raw {
        id: u64,
        slug: String,
        owner: Owner,
        pem: String,
    }
    // Not `{e}` of the body: serde's message can quote the value it choked on.
    let raw: Raw = serde_json::from_slice(&resp.body).map_err(|e| {
        InitError::Conversion(format!(
            "the answer was not the shape expected (line {}, column {})",
            e.line(),
            e.column()
        ))
    })?;
    Ok(Converted { app_id: raw.id, slug: raw.slug, owner: raw.owner.login, pem: Pem(raw.pem) })
}

/// The App, authenticated as itself.
pub struct AppAuth {
    app_id: u64,
    key: RsaKeyPair,
    rng: SystemRandom,
}

impl AppAuth {
    pub fn new(app_id: u64, pem: &Pem) -> Result<Self, InitError> {
        let bad = |m: String| InitError::Key(format!("the key GitHub returned does not load: {m}"));
        let der = PrivateKeyDer::from_pem_slice(pem.as_str().as_bytes())
            .map_err(|e| bad(e.to_string()))?;
        let key = match der {
            PrivateKeyDer::Pkcs1(der) => RsaKeyPair::from_der(der.secret_pkcs1_der()),
            PrivateKeyDer::Pkcs8(der) => RsaKeyPair::from_pkcs8(der.secret_pkcs8_der()),
            _ => return Err(bad("GitHub App keys are RSA".into())),
        }
        .map_err(|e| bad(e.to_string()))?;
        Ok(Self { app_id, key, rng: SystemRandom::new() })
    }

    fn jwt(&self, now: Wall) -> Result<String, InitError> {
        let now_s = now.0.div_euclid(1000);
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = json!({
            "iat": now_s - JWT_BACKDATE_S,
            "exp": now_s + JWT_LIFETIME_S,
            "iss": self.app_id.to_string(),
        });
        let signing_input = format!("{header}.{}", URL_SAFE_NO_PAD.encode(claims.to_string()));
        let mut sig = vec![0u8; self.key.public().modulus_len()];
        self.key
            .sign(&RSA_PKCS1_SHA256, &self.rng, signing_input.as_bytes(), &mut sig)
            .map_err(|_| InitError::Key("signing the App's JWT failed".into()))?;
        Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig)))
    }

    fn get(&self, http: &dyn Http, path: &str, now: Wall) -> Result<HttpResponse, InitError> {
        let url = format!("{API_BASE}{path}");
        let resp = http
            .get(&url, &headers(Some(self.jwt(now)?)))
            .map_err(|e| InitError::Transport(e.0))?;
        tracing::debug!(status = resp.status, path, "read the app back");
        if resp.status != 200 {
            return Err(InitError::Api { what: path.to_string(), detail: snippet(&resp) });
        }
        Ok(resp)
    }

    /// Fails unless the App GitHub created has exactly the permissions and events the manifest
    /// asked for — not a superset, which is the direction a hand-edited form drifts in.
    pub fn verify(&self, http: &dyn Http, now: Wall) -> Result<(), InitError> {
        #[derive(Deserialize)]
        struct App {
            #[serde(default)]
            permissions: BTreeMap<String, String>,
            #[serde(default)]
            events: Vec<String>,
        }
        let resp = self.get(http, "/app", now)?;
        let app: App = serde_json::from_slice(&resp.body)
            .map_err(|e| InitError::Api { what: "/app".into(), detail: e.to_string() })?;
        let want: BTreeMap<String, String> =
            PERMISSIONS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        if app.permissions != want || !app.events.is_empty() {
            return Err(InitError::Permissions {
                want: render(&want, &[]),
                got: render(&app.permissions, &app.events),
            });
        }
        Ok(())
    }

    /// The installation on `owner`'s account, once the operator has made one. `None` until then.
    pub fn installation(
        &self,
        http: &dyn Http,
        owner: &str,
        now: Wall,
    ) -> Result<Option<u64>, InitError> {
        #[derive(Deserialize)]
        struct Account {
            login: String,
        }
        #[derive(Deserialize)]
        struct Installation {
            id: u64,
            account: Account,
        }
        let resp = self.get(http, "/app/installations", now)?;
        let all: Vec<Installation> = serde_json::from_slice(&resp.body).map_err(|e| {
            InitError::Api { what: "/app/installations".into(), detail: e.to_string() }
        })?;
        Ok(all.into_iter().find(|i| i.account.login.eq_ignore_ascii_case(owner)).map(|i| i.id))
    }
}

fn render(perms: &BTreeMap<String, String>, events: &[String]) -> String {
    let perms: Vec<String> = perms.iter().map(|(k, v)| format!("{k}:{v}")).collect();
    format!("permissions [{}], events [{}]", perms.join(", "), events.join(", "))
}

/// Only for a response that failed: a failed answer carries GitHub's message, never a key.
fn snippet(resp: &HttpResponse) -> String {
    let body: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
    match body.get("message").and_then(Value::as_str) {
        Some(m) => format!("{}: {m}", resp.status),
        None => resp.status.to_string(),
    }
}
