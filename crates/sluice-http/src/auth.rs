//! Who is calling, over HTTP.
//!
//! An authenticator turns request headers into a [`Principal`]: an identity, a
//! set of candidate roles, and attributes. The server then resolves that
//! against the deployment's roles to build a [`Caller`].
//!
//! Three are supported. OIDC is the one for shared deployments: the caller's
//! token decides their role and their scope, per request, which is what stdio
//! could not do. API keys suit machine callers and small installs. `none` is
//! for local development and says so loudly.
//!
//! One rule matters more than the rest: attributes may only come from claims
//! the operator has explicitly mapped. There is no blanket copy of token
//! claims into caller attributes, because a row filter reads those attributes
//! and an identity provider that starts emitting an extra claim must not
//! silently widen someone's scope.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use http::HeaderMap;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sluice_core::Value;
use tokio::sync::RwLock;

/// Why a request could not be attributed to anyone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// No credential was presented.
    Missing,
    /// A credential was presented but could not be read.
    Malformed(String),
    /// The credential was read but is not acceptable.
    Rejected(String),
    /// The credential is valid but carries no role this deployment knows.
    NoRole(String),
    /// The key material could not be reached.
    Unavailable(String),
}

impl AuthError {
    /// The HTTP status this maps to.
    pub fn status(&self) -> u16 {
        match self {
            Self::Missing | Self::Malformed(_) | Self::Rejected(_) => 401,
            Self::NoRole(_) => 403,
            Self::Unavailable(_) => 503,
        }
    }

    /// A stable code for logs and metrics.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing => "credential_missing",
            Self::Malformed(_) => "credential_malformed",
            Self::Rejected(_) => "credential_rejected",
            Self::NoRole(_) => "no_role",
            Self::Unavailable(_) => "keys_unavailable",
        }
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("no credential was presented"),
            Self::Malformed(m) => write!(f, "the credential could not be read: {m}"),
            Self::Rejected(m) => write!(f, "the credential was rejected: {m}"),
            Self::NoRole(m) => write!(f, "no usable role: {m}"),
            Self::Unavailable(m) => write!(f, "cannot verify credentials right now: {m}"),
        }
    }
}

/// An authenticated identity, before it is matched to a configured role.
#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    /// Recorded in the audit log.
    pub id: String,
    /// Candidate roles, most preferred first.
    pub roles: Vec<String>,
    /// Attributes taken from explicitly mapped claims.
    pub attributes: BTreeMap<String, Value>,
}

/// Turns request headers into a principal.
#[async_trait]
pub trait Authenticator: fmt::Debug + Send + Sync {
    /// Identify the caller.
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Principal, AuthError>;

    /// One line for logs and `sluice doctor`, with no secrets in it.
    fn describe(&self) -> String;

    /// True when this authenticator does not actually check anything.
    fn is_open(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// No authentication
// ---------------------------------------------------------------------------

/// Treats every request as one fixed identity. Development only.
#[derive(Debug, Clone)]
pub struct OpenAuthenticator {
    role: String,
    id: String,
}

impl OpenAuthenticator {
    /// Every request becomes this role and identity.
    pub fn new(role: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            id: id.into(),
        }
    }
}

#[async_trait]
impl Authenticator for OpenAuthenticator {
    async fn authenticate(&self, _headers: &HeaderMap) -> Result<Principal, AuthError> {
        Ok(Principal {
            id: self.id.clone(),
            roles: vec![self.role.clone()],
            attributes: BTreeMap::new(),
        })
    }

    fn describe(&self) -> String {
        format!(
            "no authentication (everyone is `{}` as `{}`)",
            self.role, self.id
        )
    }

    fn is_open(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// API keys
// ---------------------------------------------------------------------------

/// One issued key.
#[derive(Debug, Clone)]
pub struct ApiKey {
    /// Lowercase hex SHA-256 of the key material.
    pub hash: String,
    /// Role this key acts as.
    pub role: String,
    /// Identity recorded in the audit log.
    pub caller: String,
    /// Attributes this key carries.
    pub attributes: BTreeMap<String, Value>,
}

/// Checks a bearer token against a list of hashed keys.
///
/// Only the hash is stored, so the configuration file never holds anything
/// that can be replayed. Comparison is constant time.
#[derive(Debug)]
pub struct ApiKeyAuthenticator {
    keys: Vec<ApiKey>,
}

impl ApiKeyAuthenticator {
    /// Build from the issued keys.
    pub fn new(keys: Vec<ApiKey>) -> Self {
        Self { keys }
    }

    /// The hash to put in a configuration file for this key.
    pub fn hash(key: &str) -> String {
        let mut h = Sha256::new();
        h.update(key.as_bytes());
        h.finalize().iter().fold(String::new(), |mut s, b| {
            use fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
    }
}

#[async_trait]
impl Authenticator for ApiKeyAuthenticator {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Principal, AuthError> {
        let presented = bearer(headers)?;
        let digest = Self::hash(&presented);

        // Constant-time over the whole list: a timing difference would say
        // which prefix was right.
        let mut found: Option<&ApiKey> = None;
        for key in &self.keys {
            if constant_time_eq(key.hash.as_bytes(), digest.as_bytes()) {
                found = Some(key);
            }
        }
        let key = found.ok_or_else(|| AuthError::Rejected("unknown API key".into()))?;

        Ok(Principal {
            id: key.caller.clone(),
            roles: vec![key.role.clone()],
            attributes: key.attributes.clone(),
        })
    }

    fn describe(&self) -> String {
        format!("API keys ({} issued)", self.keys.len())
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn bearer(headers: &HeaderMap) -> Result<String, AuthError> {
    let value = headers
        .get(http::header::AUTHORIZATION)
        .ok_or(AuthError::Missing)?
        .to_str()
        .map_err(|_| AuthError::Malformed("the Authorization header is not text".into()))?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .ok_or_else(|| AuthError::Malformed("expected `Authorization: Bearer <token>`".into()))?;
    if token.trim().is_empty() {
        return Err(AuthError::Malformed("the bearer token is empty".into()));
    }
    Ok(token.trim().to_owned())
}

// ---------------------------------------------------------------------------
// OIDC
// ---------------------------------------------------------------------------

/// How to validate tokens and what to read out of them.
#[derive(Debug, Clone)]
pub struct OidcConfig {
    /// Expected `iss`.
    pub issuer: String,
    /// Accepted `aud` values.
    pub audience: Vec<String>,
    /// Claim holding the role, as a string or an array of strings.
    pub role_claim: String,
    /// Claim holding the caller identity.
    pub caller_claim: String,
    /// Attribute name to claim name. Nothing else becomes an attribute.
    pub attribute_claims: BTreeMap<String, String>,
    /// Claim value to deployment role. Unmapped values are used as-is.
    pub role_map: BTreeMap<String, String>,
    /// Clock skew allowance.
    pub leeway: Duration,
    /// How long a fetched key set is reused.
    pub refresh_interval: Duration,
}

impl Default for OidcConfig {
    fn default() -> Self {
        Self {
            issuer: String::new(),
            audience: Vec::new(),
            role_claim: "sluice_role".into(),
            caller_claim: "sub".into(),
            attribute_claims: BTreeMap::new(),
            role_map: BTreeMap::new(),
            leeway: Duration::from_secs(60),
            refresh_interval: Duration::from_secs(300),
        }
    }
}

/// One key from a JWKS document.
#[derive(Debug, Clone, Deserialize)]
pub struct Jwk {
    /// Key type: RSA, EC or oct.
    pub kty: String,
    /// Key id.
    #[serde(default)]
    pub kid: Option<String>,
    /// Algorithm.
    #[serde(default)]
    pub alg: Option<String>,
    /// RSA modulus.
    #[serde(default)]
    pub n: Option<String>,
    /// RSA exponent.
    #[serde(default)]
    pub e: Option<String>,
    /// EC curve.
    #[serde(default)]
    pub crv: Option<String>,
    /// EC x coordinate.
    #[serde(default)]
    pub x: Option<String>,
    /// EC y coordinate.
    #[serde(default)]
    pub y: Option<String>,
    /// Symmetric key material.
    #[serde(default)]
    pub k: Option<String>,
}

/// A JWKS document.
#[derive(Debug, Clone, Deserialize)]
pub struct JwkSet {
    /// The keys.
    pub keys: Vec<Jwk>,
}

/// Where the verification keys come from.
#[async_trait]
pub trait KeySource: fmt::Debug + Send + Sync {
    /// Fetch the current key set.
    async fn fetch(&self) -> Result<JwkSet, String>;
    /// For diagnostics.
    fn describe(&self) -> String;
}

/// The one field Sluice needs from an OIDC discovery document.
#[derive(Debug, Deserialize)]
struct Discovery {
    jwks_uri: String,
}

/// Fetches a JWKS over HTTPS.
#[derive(Debug)]
pub struct HttpKeySource {
    url: String,
    client: reqwest::Client,
}

impl HttpKeySource {
    /// Point at a JWKS endpoint.
    pub fn new(url: impl Into<String>) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("sluice/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("cannot build an HTTP client: {e}"))?;
        Ok(Self {
            url: url.into(),
            client,
        })
    }

    /// Find the JWKS endpoint from the issuer's discovery document.
    pub async fn discover(issuer: &str) -> Result<Self, String> {
        let base = issuer.trim_end_matches('/');
        let url = format!("{base}/.well-known/openid-configuration");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("cannot build an HTTP client: {e}"))?;
        let doc: Discovery = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("cannot reach {url}: {e}"))?
            .error_for_status()
            .map_err(|e| format!("{url} answered {e}"))?
            .json()
            .await
            .map_err(|e| format!("{url} is not a discovery document: {e}"))?;
        Self::new(doc.jwks_uri)
    }
}

#[async_trait]
impl KeySource for HttpKeySource {
    async fn fetch(&self) -> Result<JwkSet, String> {
        self.client
            .get(&self.url)
            .send()
            .await
            .map_err(|e| format!("cannot reach {}: {e}", self.url))?
            .error_for_status()
            .map_err(|e| format!("{} answered {e}", self.url))?
            .json()
            .await
            .map_err(|e| format!("{} is not a JWKS: {e}", self.url))
    }

    fn describe(&self) -> String {
        self.url.clone()
    }
}

/// A fixed key set, for tests and for air-gapped deployments that ship the
/// key material alongside the configuration.
#[derive(Debug, Clone)]
pub struct StaticKeySource(pub JwkSet);

#[async_trait]
impl KeySource for StaticKeySource {
    async fn fetch(&self) -> Result<JwkSet, String> {
        Ok(self.0.clone())
    }

    fn describe(&self) -> String {
        format!("{} static key(s)", self.0.keys.len())
    }
}

#[derive(Debug)]
struct CachedKeys {
    fetched: Option<Instant>,
    keys: Vec<(Option<String>, Algorithm, DecodingKey)>,
}

/// Validates OIDC access tokens and maps their claims onto a principal.
#[derive(Debug)]
pub struct OidcAuthenticator {
    config: OidcConfig,
    source: Arc<dyn KeySource>,
    cache: RwLock<CachedKeys>,
}

impl OidcAuthenticator {
    /// Build an authenticator over a key source.
    pub fn new(config: OidcConfig, source: Arc<dyn KeySource>) -> Self {
        Self {
            config,
            source,
            cache: RwLock::new(CachedKeys {
                fetched: None,
                keys: Vec::new(),
            }),
        }
    }

    /// Fetch the key set now, so a misconfiguration surfaces at startup rather
    /// than on the first request.
    pub async fn warm(&self) -> Result<usize, String> {
        self.refresh().await
    }

    async fn refresh(&self) -> Result<usize, String> {
        let set = self.source.fetch().await?;
        let mut keys = Vec::new();
        for jwk in set.keys {
            match decode_jwk(&jwk) {
                Ok(entry) => keys.push(entry),
                // One unusable key should not poison a whole rotation.
                Err(e) => tracing::warn!(kid = ?jwk.kid, "ignoring a key in the JWKS: {e}"),
            }
        }
        if keys.is_empty() {
            return Err("the key set contains no usable keys".into());
        }
        let count = keys.len();
        let mut cache = self.cache.write().await;
        cache.keys = keys;
        cache.fetched = Some(Instant::now());
        Ok(count)
    }

    /// Keys to try for a token, refreshing when stale or when the kid is new.
    async fn keys_for(
        &self,
        kid: Option<&str>,
    ) -> Result<Vec<(Algorithm, DecodingKey)>, AuthError> {
        let stale = {
            let cache = self.cache.read().await;
            match cache.fetched {
                None => true,
                Some(at) => at.elapsed() > self.config.refresh_interval,
            }
        };
        let unknown_kid = {
            let cache = self.cache.read().await;
            kid.is_some_and(|k| !cache.keys.iter().any(|(id, _, _)| id.as_deref() == Some(k)))
        };
        if stale || unknown_kid {
            // A rotation publishes new keys under new ids, so an unknown kid
            // is the normal signal to refetch rather than an attack.
            if let Err(e) = self.refresh().await {
                let cache = self.cache.read().await;
                if cache.keys.is_empty() {
                    return Err(AuthError::Unavailable(e));
                }
                tracing::warn!("keeping the cached key set: {e}");
            }
        }

        let cache = self.cache.read().await;
        let matching: Vec<(Algorithm, DecodingKey)> = cache
            .keys
            .iter()
            .filter(|(id, _, _)| match (kid, id) {
                (Some(want), Some(have)) => want == have,
                // A token with no kid, or a key with no kid, has to be tried.
                _ => true,
            })
            .map(|(_, alg, key)| (*alg, key.clone()))
            .collect();

        if matching.is_empty() {
            return Err(AuthError::Rejected(
                "the token was signed with a key this server does not have".into(),
            ));
        }
        Ok(matching)
    }
}

fn decode_jwk(jwk: &Jwk) -> Result<(Option<String>, Algorithm, DecodingKey), String> {
    let alg = match jwk.alg.as_deref() {
        Some(a) => a
            .parse::<Algorithm>()
            .map_err(|_| format!("unsupported algorithm `{a}`"))?,
        None => match jwk.kty.as_str() {
            "RSA" => Algorithm::RS256,
            "EC" => Algorithm::ES256,
            "oct" => Algorithm::HS256,
            other => return Err(format!("unsupported key type `{other}`")),
        },
    };
    let key = match jwk.kty.as_str() {
        "RSA" => {
            let n = jwk.n.as_deref().ok_or("an RSA key needs `n`")?;
            let e = jwk.e.as_deref().ok_or("an RSA key needs `e`")?;
            DecodingKey::from_rsa_components(n, e).map_err(|e| format!("bad RSA key: {e}"))?
        }
        "EC" => {
            let x = jwk.x.as_deref().ok_or("an EC key needs `x`")?;
            let y = jwk.y.as_deref().ok_or("an EC key needs `y`")?;
            DecodingKey::from_ec_components(x, y).map_err(|e| format!("bad EC key: {e}"))?
        }
        "oct" => {
            let k = jwk.k.as_deref().ok_or("a symmetric key needs `k`")?;
            DecodingKey::from_base64_secret(k).map_err(|e| format!("bad symmetric key: {e}"))?
        }
        other => return Err(format!("unsupported key type `{other}`")),
    };
    Ok((jwk.kid.clone(), alg, key))
}

#[async_trait]
impl Authenticator for OidcAuthenticator {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Principal, AuthError> {
        let token = bearer(headers)?;
        let header = jsonwebtoken::decode_header(&token)
            .map_err(|e| AuthError::Malformed(format!("not a JWT: {e}")))?;
        let candidates = self.keys_for(header.kid.as_deref()).await?;

        let mut last = None;
        let mut claims: Option<serde_json::Value> = None;
        for (alg, key) in candidates {
            // The algorithm comes from the key, never from the token header:
            // trusting the header is how `alg: none` and HMAC-with-the-public-
            // key attacks work.
            let mut validation = Validation::new(alg);
            validation.leeway = self.config.leeway.as_secs();
            if !self.config.audience.is_empty() {
                validation.set_audience(&self.config.audience);
            }
            if !self.config.issuer.is_empty() {
                validation.set_issuer(&[self.config.issuer.as_str()]);
            }
            match jsonwebtoken::decode::<serde_json::Value>(&token, &key, &validation) {
                Ok(data) => {
                    claims = Some(data.claims);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let Some(claims) = claims else {
            let message = last.map_or_else(
                || "no key accepted the token".to_owned(),
                |e| describe_jwt_error(&e),
            );
            return Err(AuthError::Rejected(message));
        };

        let id = claims
            .get(&self.config.caller_claim)
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AuthError::Rejected(format!(
                    "the token has no `{}` claim to identify the caller",
                    self.config.caller_claim
                ))
            })?
            .to_owned();

        let roles = read_roles(&claims, &self.config);
        if roles.is_empty() {
            return Err(AuthError::NoRole(format!(
                "the token's `{}` claim is missing or empty",
                self.config.role_claim
            )));
        }

        let mut attributes = BTreeMap::new();
        for (attribute, claim) in &self.config.attribute_claims {
            if let Some(v) = claims.get(claim) {
                attributes.insert(attribute.clone(), json_to_value(v));
            }
        }

        Ok(Principal {
            id,
            roles,
            attributes,
        })
    }

    fn describe(&self) -> String {
        format!(
            "OIDC (issuer {}, keys from {})",
            if self.config.issuer.is_empty() {
                "unchecked"
            } else {
                &self.config.issuer
            },
            self.source.describe()
        )
    }
}

fn read_roles(claims: &serde_json::Value, config: &OidcConfig) -> Vec<String> {
    let map = |raw: &str| -> String {
        config
            .role_map
            .get(raw)
            .cloned()
            .unwrap_or_else(|| raw.to_owned())
    };
    match claims.get(&config.role_claim) {
        Some(serde_json::Value::String(s)) => {
            // Space-separated is common in scope-style claims.
            s.split_whitespace().map(map).collect()
        }
        Some(serde_json::Value::Array(items)) => {
            items.iter().filter_map(|v| v.as_str()).map(map).collect()
        }
        _ => Vec::new(),
    }
}

fn json_to_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::String(s) => Value::Text(s.clone()),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map_or_else(|| n.as_f64().map_or(Value::Null, Value::Float), Value::Int),
        other => Value::Json(other.clone()),
    }
}

/// Say what is wrong with a token without echoing the token back.
fn describe_jwt_error(e: &jsonwebtoken::errors::Error) -> String {
    use jsonwebtoken::errors::ErrorKind as K;
    match e.kind() {
        K::ExpiredSignature => "the token has expired".into(),
        K::ImmatureSignature => "the token is not valid yet".into(),
        K::InvalidAudience => "the token is for a different audience".into(),
        K::InvalidIssuer => "the token is from a different issuer".into(),
        K::InvalidSignature => "the signature does not verify".into(),
        K::InvalidAlgorithm => "the token uses an algorithm this key cannot verify".into(),
        _ => "the token is not acceptable".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header};
    use serde_json::json;

    const TEST_KEY: &str = include_str!("../tests/fixtures/testing-only-rsa-key.pem");
    const TEST_JWKS: &str = include_str!("../tests/fixtures/testing-only-jwks.json");

    fn headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        h
    }

    fn sign(claims: &serde_json::Value) -> String {
        let pem: String = TEST_KEY
            .lines()
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        let key = EncodingKey::from_rsa_pem(pem.as_bytes()).expect("test key should load");
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-key-1".into());
        jsonwebtoken::encode(&header, claims, &key).expect("should sign")
    }

    fn authenticator() -> OidcAuthenticator {
        let set: JwkSet = serde_json::from_str(TEST_JWKS).expect("fixture JWKS should parse");
        let mut config = OidcConfig {
            issuer: "https://id.example.com/".into(),
            audience: vec!["sluice".into()],
            role_claim: "sluice_role".into(),
            caller_claim: "sub".into(),
            ..OidcConfig::default()
        };
        config
            .attribute_claims
            .insert("region".into(), "region".into());
        config
            .role_map
            .insert("support-eu".into(), "support_eu".into());
        OidcAuthenticator::new(config, Arc::new(StaticKeySource(set)))
    }

    fn future() -> i64 {
        jiff::Timestamp::now().as_second() + 600
    }

    #[tokio::test]
    async fn a_valid_token_becomes_a_principal() {
        let token = sign(&json!({
            "iss": "https://id.example.com/",
            "aud": "sluice",
            "sub": "alice@example.com",
            "exp": future(),
            "sluice_role": "support-eu",
            "region": "EU",
        }));
        let p = authenticator()
            .authenticate(&headers(&token))
            .await
            .unwrap();
        assert_eq!(p.id, "alice@example.com");
        assert_eq!(
            p.roles,
            vec!["support_eu".to_string()],
            "role_map should apply"
        );
        assert_eq!(p.attributes.get("region"), Some(&Value::Text("EU".into())));
    }

    #[tokio::test]
    async fn only_mapped_claims_become_attributes() {
        let token = sign(&json!({
            "iss": "https://id.example.com/",
            "aud": "sluice",
            "sub": "alice",
            "exp": future(),
            "sluice_role": "support-eu",
            "region": "EU",
            "tenant": "acme",
            "is_admin": true,
        }));
        let p = authenticator()
            .authenticate(&headers(&token))
            .await
            .unwrap();
        assert_eq!(p.attributes.len(), 1, "only `region` is mapped");
        assert!(p.attributes.contains_key("region"));
    }

    #[tokio::test]
    async fn an_expired_token_is_refused() {
        let token = sign(&json!({
            "iss": "https://id.example.com/",
            "aud": "sluice",
            "sub": "alice",
            "exp": jiff::Timestamp::now().as_second() - 3600,
            "sluice_role": "support-eu",
        }));
        let err = authenticator()
            .authenticate(&headers(&token))
            .await
            .unwrap_err();
        assert_eq!(err.status(), 401);
        assert!(format!("{err}").contains("expired"), "{err}");
    }

    #[tokio::test]
    async fn the_wrong_audience_is_refused() {
        let token = sign(&json!({
            "iss": "https://id.example.com/",
            "aud": "someone-else",
            "sub": "alice",
            "exp": future(),
            "sluice_role": "support-eu",
        }));
        let err = authenticator()
            .authenticate(&headers(&token))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("audience"), "{err}");
    }

    #[tokio::test]
    async fn the_wrong_issuer_is_refused() {
        let token = sign(&json!({
            "iss": "https://evil.example.com/",
            "aud": "sluice",
            "sub": "alice",
            "exp": future(),
            "sluice_role": "support-eu",
        }));
        let err = authenticator()
            .authenticate(&headers(&token))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("issuer"), "{err}");
    }

    #[tokio::test]
    async fn an_unsigned_token_is_refused() {
        // `alg: none` with the signature stripped, the classic JWT attack.
        let header = URL_SAFE_NO_PAD_ENCODE(br#"{"alg":"none","typ":"JWT"}"#);
        let claims = URL_SAFE_NO_PAD_ENCODE(
            br#"{"iss":"https://id.example.com/","aud":"sluice","sub":"mallory","exp":99999999999,"sluice_role":"support-eu"}"#,
        );
        let token = format!("{header}.{claims}.");
        let err = authenticator()
            .authenticate(&headers(&token))
            .await
            .unwrap_err();
        assert_eq!(err.status(), 401, "{err}");
    }

    #[tokio::test]
    async fn a_token_with_no_role_claim_is_forbidden_not_unauthorised() {
        let token = sign(&json!({
            "iss": "https://id.example.com/",
            "aud": "sluice",
            "sub": "alice",
            "exp": future(),
        }));
        let err = authenticator()
            .authenticate(&headers(&token))
            .await
            .unwrap_err();
        assert_eq!(err.status(), 403);
    }

    #[tokio::test]
    async fn a_missing_header_is_reported_as_missing() {
        let err = authenticator()
            .authenticate(&HeaderMap::new())
            .await
            .unwrap_err();
        assert_eq!(err, AuthError::Missing);
    }

    #[tokio::test]
    async fn api_keys_match_on_a_hash_and_nothing_else() {
        let key = "sk_live_example_value";
        let auth = ApiKeyAuthenticator::new(vec![ApiKey {
            hash: ApiKeyAuthenticator::hash(key),
            role: "batch".into(),
            caller: "nightly-job".into(),
            attributes: BTreeMap::from([("region".to_string(), Value::Text("EU".into()))]),
        }]);

        let p = auth.authenticate(&headers(key)).await.unwrap();
        assert_eq!(p.id, "nightly-job");
        assert_eq!(p.roles, vec!["batch".to_string()]);

        let err = auth
            .authenticate(&headers("sk_live_wrong"))
            .await
            .unwrap_err();
        assert_eq!(err.status(), 401);
    }

    #[test]
    fn roles_can_arrive_as_a_string_a_list_or_a_scope() {
        let mut config = OidcConfig::default();
        config.role_map.insert("a".into(), "role_a".into());
        assert_eq!(
            read_roles(&json!({ "sluice_role": "a" }), &config),
            vec!["role_a".to_string()]
        );
        assert_eq!(
            read_roles(&json!({ "sluice_role": ["a", "b"] }), &config),
            vec!["role_a".to_string(), "b".to_string()]
        );
        assert_eq!(
            read_roles(&json!({ "sluice_role": "a b" }), &config),
            vec!["role_a".to_string(), "b".to_string()]
        );
        assert!(read_roles(&json!({}), &config).is_empty());
    }

    #[test]
    fn jwks_entries_that_cannot_be_used_are_skipped_not_fatal() {
        let set: JwkSet = serde_json::from_str(
            r#"{"keys":[{"kty":"RSA","kid":"broken"},{"kty":"oct","kid":"ok","k":"c2VjcmV0LXZhbHVl"}]}"#,
        )
        .unwrap();
        assert!(decode_jwk(&set.keys[0]).is_err());
        assert!(decode_jwk(&set.keys[1]).is_ok());
    }

    #[allow(non_snake_case)]
    fn URL_SAFE_NO_PAD_ENCODE(bytes: &[u8]) -> String {
        // Minimal base64url, so the test does not pull in a dependency.
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            let take = chunk.len() + 1;
            for i in 0..take {
                out.push(T[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            }
        }
        out
    }
}
