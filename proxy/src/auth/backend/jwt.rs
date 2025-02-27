use std::{
    future::Future,
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::{bail, ensure, Context};
use arc_swap::ArcSwapOption;
use dashmap::DashMap;
use jose_jwk::crypto::KeyInfo;
use serde::{Deserialize, Deserializer};
use signature::Verifier;
use tokio::time::Instant;

use crate::{context::RequestMonitoring, http::parse_json_body_with_limit, EndpointId, RoleName};

// TODO(conrad): make these configurable.
const CLOCK_SKEW_LEEWAY: Duration = Duration::from_secs(30);
const MIN_RENEW: Duration = Duration::from_secs(30);
const AUTO_RENEW: Duration = Duration::from_secs(300);
const MAX_RENEW: Duration = Duration::from_secs(3600);
const MAX_JWK_BODY_SIZE: usize = 64 * 1024;

/// How to get the JWT auth rules
pub trait FetchAuthRules: Clone + Send + Sync + 'static {
    fn fetch_auth_rules(
        &self,
        role_name: RoleName,
    ) -> impl Future<Output = anyhow::Result<Vec<AuthRule>>> + Send;
}

pub struct AuthRule {
    pub id: String,
    pub jwks_url: url::Url,
    pub audience: Option<String>,
}

#[derive(Default)]
pub struct JwkCache {
    client: reqwest::Client,

    map: DashMap<(EndpointId, RoleName), Arc<JwkCacheEntryLock>>,
}

pub struct JwkCacheEntry {
    /// Should refetch at least every hour to verify when old keys have been removed.
    /// Should refetch when new key IDs are seen only every 5 minutes or so
    last_retrieved: Instant,

    /// cplane will return multiple JWKs urls that we need to scrape.
    key_sets: ahash::HashMap<String, KeySet>,
}

impl JwkCacheEntry {
    fn find_jwk_and_audience(&self, key_id: &str) -> Option<(&jose_jwk::Jwk, Option<&str>)> {
        self.key_sets.values().find_map(|key_set| {
            key_set
                .find_key(key_id)
                .map(|jwk| (jwk, key_set.audience.as_deref()))
        })
    }
}

struct KeySet {
    jwks: jose_jwk::JwkSet,
    audience: Option<String>,
}

impl KeySet {
    fn find_key(&self, key_id: &str) -> Option<&jose_jwk::Jwk> {
        self.jwks
            .keys
            .iter()
            .find(|jwk| jwk.prm.kid.as_deref() == Some(key_id))
    }
}

pub struct JwkCacheEntryLock {
    cached: ArcSwapOption<JwkCacheEntry>,
    lookup: tokio::sync::Semaphore,
}

impl Default for JwkCacheEntryLock {
    fn default() -> Self {
        JwkCacheEntryLock {
            cached: ArcSwapOption::empty(),
            lookup: tokio::sync::Semaphore::new(1),
        }
    }
}

impl JwkCacheEntryLock {
    async fn acquire_permit<'a>(self: &'a Arc<Self>) -> JwkRenewalPermit<'a> {
        JwkRenewalPermit::acquire_permit(self).await
    }

    fn try_acquire_permit<'a>(self: &'a Arc<Self>) -> Option<JwkRenewalPermit<'a>> {
        JwkRenewalPermit::try_acquire_permit(self)
    }

    async fn renew_jwks<F: FetchAuthRules>(
        &self,
        _permit: JwkRenewalPermit<'_>,
        client: &reqwest::Client,
        role_name: RoleName,
        auth_rules: &F,
    ) -> anyhow::Result<Arc<JwkCacheEntry>> {
        // double check that no one beat us to updating the cache.
        let now = Instant::now();
        let guard = self.cached.load_full();
        if let Some(cached) = guard {
            let last_update = now.duration_since(cached.last_retrieved);
            if last_update < Duration::from_secs(300) {
                return Ok(cached);
            }
        }

        let rules = auth_rules.fetch_auth_rules(role_name).await?;
        let mut key_sets =
            ahash::HashMap::with_capacity_and_hasher(rules.len(), ahash::RandomState::new());
        // TODO(conrad): run concurrently
        // TODO(conrad): strip the JWKs urls (should be checked by cplane as well - cloud#16284)
        for rule in rules {
            let req = client.get(rule.jwks_url.clone());
            // TODO(conrad): eventually switch to using reqwest_middleware/`new_client_with_timeout`.
            // TODO(conrad): We need to filter out URLs that point to local resources. Public internet only.
            match req.send().await.and_then(|r| r.error_for_status()) {
                // todo: should we re-insert JWKs if we want to keep this JWKs URL?
                // I expect these failures would be quite sparse.
                Err(e) => tracing::warn!(url=?rule.jwks_url, error=?e, "could not fetch JWKs"),
                Ok(r) => {
                    let resp: http::Response<reqwest::Body> = r.into();
                    match parse_json_body_with_limit::<jose_jwk::JwkSet>(
                        resp.into_body(),
                        MAX_JWK_BODY_SIZE,
                    )
                    .await
                    {
                        Err(e) => {
                            tracing::warn!(url=?rule.jwks_url, error=?e, "could not decode JWKs");
                        }
                        Ok(jwks) => {
                            key_sets.insert(
                                rule.id,
                                KeySet {
                                    jwks,
                                    audience: rule.audience,
                                },
                            );
                        }
                    }
                }
            }
        }

        let entry = Arc::new(JwkCacheEntry {
            last_retrieved: now,
            key_sets,
        });
        self.cached.swap(Some(Arc::clone(&entry)));

        Ok(entry)
    }

    async fn get_or_update_jwk_cache<F: FetchAuthRules>(
        self: &Arc<Self>,
        ctx: &RequestMonitoring,
        client: &reqwest::Client,
        role_name: RoleName,
        fetch: &F,
    ) -> Result<Arc<JwkCacheEntry>, anyhow::Error> {
        let now = Instant::now();
        let guard = self.cached.load_full();

        // if we have no cached JWKs, try and get some
        let Some(cached) = guard else {
            let _paused = ctx.latency_timer_pause(crate::metrics::Waiting::Compute);
            let permit = self.acquire_permit().await;
            return self.renew_jwks(permit, client, role_name, fetch).await;
        };

        let last_update = now.duration_since(cached.last_retrieved);

        // check if the cached JWKs need updating.
        if last_update > MAX_RENEW {
            let _paused = ctx.latency_timer_pause(crate::metrics::Waiting::Compute);
            let permit = self.acquire_permit().await;

            // it's been too long since we checked the keys. wait for them to update.
            return self.renew_jwks(permit, client, role_name, fetch).await;
        }

        // every 5 minutes we should spawn a job to eagerly update the token.
        if last_update > AUTO_RENEW {
            if let Some(permit) = self.try_acquire_permit() {
                tracing::debug!("JWKs should be renewed. Renewal permit acquired");
                let permit = permit.into_owned();
                let entry = self.clone();
                let client = client.clone();
                let fetch = fetch.clone();
                tokio::spawn(async move {
                    if let Err(e) = entry.renew_jwks(permit, &client, role_name, &fetch).await {
                        tracing::warn!(error=?e, "could not fetch JWKs in background job");
                    }
                });
            } else {
                tracing::debug!("JWKs should be renewed. Renewal permit already taken, skipping");
            }
        }

        Ok(cached)
    }

    async fn check_jwt<F: FetchAuthRules>(
        self: &Arc<Self>,
        ctx: &RequestMonitoring,
        jwt: &str,
        client: &reqwest::Client,
        role_name: RoleName,
        fetch: &F,
    ) -> Result<(), anyhow::Error> {
        // JWT compact form is defined to be
        // <B64(Header)> || . || <B64(Payload)> || . || <B64(Signature)>
        // where Signature = alg(<B64(Header)> || . || <B64(Payload)>);

        let (header_payload, signature) = jwt
            .rsplit_once(".")
            .context("Provided authentication token is not a valid JWT encoding")?;
        let (header, payload) = header_payload
            .split_once(".")
            .context("Provided authentication token is not a valid JWT encoding")?;

        let header = base64::decode_config(header, base64::URL_SAFE_NO_PAD)
            .context("Provided authentication token is not a valid JWT encoding")?;
        let header = serde_json::from_slice::<JwtHeader<'_>>(&header)
            .context("Provided authentication token is not a valid JWT encoding")?;

        let sig = base64::decode_config(signature, base64::URL_SAFE_NO_PAD)
            .context("Provided authentication token is not a valid JWT encoding")?;

        ensure!(header.typ == "JWT");
        let kid = header.key_id.context("missing key id")?;

        let mut guard = self
            .get_or_update_jwk_cache(ctx, client, role_name.clone(), fetch)
            .await?;

        // get the key from the JWKs if possible. If not, wait for the keys to update.
        let (jwk, expected_audience) = loop {
            match guard.find_jwk_and_audience(kid) {
                Some(jwk) => break jwk,
                None if guard.last_retrieved.elapsed() > MIN_RENEW => {
                    let _paused = ctx.latency_timer_pause(crate::metrics::Waiting::Compute);

                    let permit = self.acquire_permit().await;
                    guard = self
                        .renew_jwks(permit, client, role_name.clone(), fetch)
                        .await?;
                }
                _ => {
                    bail!("jwk not found");
                }
            }
        };

        ensure!(
            jwk.is_supported(&header.algorithm),
            "signature algorithm not supported"
        );

        match &jwk.key {
            jose_jwk::Key::Ec(key) => {
                verify_ec_signature(header_payload.as_bytes(), &sig, key)?;
            }
            jose_jwk::Key::Rsa(key) => {
                verify_rsa_signature(header_payload.as_bytes(), &sig, key, &jwk.prm.alg)?;
            }
            key => bail!("unsupported key type {key:?}"),
        };

        let payload = base64::decode_config(payload, base64::URL_SAFE_NO_PAD)
            .context("Provided authentication token is not a valid JWT encoding")?;
        let payload = serde_json::from_slice::<JwtPayload<'_>>(&payload)
            .context("Provided authentication token is not a valid JWT encoding")?;

        tracing::debug!(?payload, "JWT signature valid with claims");

        match (expected_audience, payload.audience) {
            // check the audience matches
            (Some(aud1), Some(aud2)) => ensure!(aud1 == aud2, "invalid JWT token audience"),
            // the audience is expected but is missing
            (Some(_), None) => bail!("invalid JWT token audience"),
            // we don't care for the audience field
            (None, _) => {}
        }

        let now = SystemTime::now();

        if let Some(exp) = payload.expiration {
            ensure!(now < exp + CLOCK_SKEW_LEEWAY);
        }

        if let Some(nbf) = payload.not_before {
            ensure!(nbf < now + CLOCK_SKEW_LEEWAY);
        }

        Ok(())
    }
}

impl JwkCache {
    pub async fn check_jwt<F: FetchAuthRules>(
        &self,
        ctx: &RequestMonitoring,
        endpoint: EndpointId,
        role_name: RoleName,
        fetch: &F,
        jwt: &str,
    ) -> Result<(), anyhow::Error> {
        // try with just a read lock first
        let key = (endpoint, role_name.clone());
        let entry = self.map.get(&key).as_deref().map(Arc::clone);
        let entry = match entry {
            Some(entry) => entry,
            None => {
                // acquire a write lock after to insert.
                let entry = self.map.entry(key).or_default();
                Arc::clone(&*entry)
            }
        };

        entry
            .check_jwt(ctx, jwt, &self.client, role_name, fetch)
            .await
    }
}

fn verify_ec_signature(data: &[u8], sig: &[u8], key: &jose_jwk::Ec) -> anyhow::Result<()> {
    use ecdsa::Signature;
    use signature::Verifier;

    match key.crv {
        jose_jwk::EcCurves::P256 => {
            let pk =
                p256::PublicKey::try_from(key).map_err(|_| anyhow::anyhow!("invalid P256 key"))?;
            let key = p256::ecdsa::VerifyingKey::from(&pk);
            let sig = Signature::from_slice(sig)?;
            key.verify(data, &sig)?;
        }
        key => bail!("unsupported ec key type {key:?}"),
    }

    Ok(())
}

fn verify_rsa_signature(
    data: &[u8],
    sig: &[u8],
    key: &jose_jwk::Rsa,
    alg: &Option<jose_jwa::Algorithm>,
) -> anyhow::Result<()> {
    use jose_jwa::{Algorithm, Signing};
    use rsa::{
        pkcs1v15::{Signature, VerifyingKey},
        RsaPublicKey,
    };

    let key = RsaPublicKey::try_from(key).map_err(|_| anyhow::anyhow!("invalid RSA key"))?;

    match alg {
        Some(Algorithm::Signing(Signing::Rs256)) => {
            let key = VerifyingKey::<sha2::Sha256>::new(key);
            let sig = Signature::try_from(sig)?;
            key.verify(data, &sig)?;
        }
        _ => bail!("invalid RSA signing algorithm"),
    };

    Ok(())
}

/// <https://datatracker.ietf.org/doc/html/rfc7515#section-4.1>
#[derive(serde::Deserialize, serde::Serialize)]
struct JwtHeader<'a> {
    /// must be "JWT"
    #[serde(rename = "typ")]
    typ: &'a str,
    /// must be a supported alg
    #[serde(rename = "alg")]
    algorithm: jose_jwa::Algorithm,
    /// key id, must be provided for our usecase
    #[serde(rename = "kid")]
    key_id: Option<&'a str>,
}

/// <https://datatracker.ietf.org/doc/html/rfc7519#section-4.1>
#[derive(serde::Deserialize, serde::Serialize, Debug)]
struct JwtPayload<'a> {
    /// Audience - Recipient for which the JWT is intended
    #[serde(rename = "aud")]
    audience: Option<&'a str>,
    /// Expiration - Time after which the JWT expires
    #[serde(deserialize_with = "numeric_date_opt", rename = "exp", default)]
    expiration: Option<SystemTime>,
    /// Not before - Time after which the JWT expires
    #[serde(deserialize_with = "numeric_date_opt", rename = "nbf", default)]
    not_before: Option<SystemTime>,

    // the following entries are only extracted for the sake of debug logging.
    /// Issuer of the JWT
    #[serde(rename = "iss")]
    issuer: Option<&'a str>,
    /// Subject of the JWT (the user)
    #[serde(rename = "sub")]
    subject: Option<&'a str>,
    /// Unique token identifier
    #[serde(rename = "jti")]
    jwt_id: Option<&'a str>,
    /// Unique session identifier
    #[serde(rename = "sid")]
    session_id: Option<&'a str>,
}

fn numeric_date_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<SystemTime>, D::Error> {
    let d = <Option<u64>>::deserialize(d)?;
    Ok(d.map(|n| SystemTime::UNIX_EPOCH + Duration::from_secs(n)))
}

struct JwkRenewalPermit<'a> {
    inner: Option<JwkRenewalPermitInner<'a>>,
}

enum JwkRenewalPermitInner<'a> {
    Owned(Arc<JwkCacheEntryLock>),
    Borrowed(&'a Arc<JwkCacheEntryLock>),
}

impl JwkRenewalPermit<'_> {
    fn into_owned(mut self) -> JwkRenewalPermit<'static> {
        JwkRenewalPermit {
            inner: self.inner.take().map(JwkRenewalPermitInner::into_owned),
        }
    }

    async fn acquire_permit(from: &Arc<JwkCacheEntryLock>) -> JwkRenewalPermit<'_> {
        match from.lookup.acquire().await {
            Ok(permit) => {
                permit.forget();
                JwkRenewalPermit {
                    inner: Some(JwkRenewalPermitInner::Borrowed(from)),
                }
            }
            Err(_) => panic!("semaphore should not be closed"),
        }
    }

    fn try_acquire_permit(from: &Arc<JwkCacheEntryLock>) -> Option<JwkRenewalPermit<'_>> {
        match from.lookup.try_acquire() {
            Ok(permit) => {
                permit.forget();
                Some(JwkRenewalPermit {
                    inner: Some(JwkRenewalPermitInner::Borrowed(from)),
                })
            }
            Err(tokio::sync::TryAcquireError::NoPermits) => None,
            Err(tokio::sync::TryAcquireError::Closed) => panic!("semaphore should not be closed"),
        }
    }
}

impl JwkRenewalPermitInner<'_> {
    fn into_owned(self) -> JwkRenewalPermitInner<'static> {
        match self {
            JwkRenewalPermitInner::Owned(p) => JwkRenewalPermitInner::Owned(p),
            JwkRenewalPermitInner::Borrowed(p) => JwkRenewalPermitInner::Owned(Arc::clone(p)),
        }
    }
}

impl Drop for JwkRenewalPermit<'_> {
    fn drop(&mut self) {
        let entry = match &self.inner {
            None => return,
            Some(JwkRenewalPermitInner::Owned(p)) => p,
            Some(JwkRenewalPermitInner::Borrowed(p)) => *p,
        };
        entry.lookup.add_permits(1);
    }
}

#[cfg(test)]
mod tests {
    use crate::RoleName;

    use super::*;

    use std::{future::IntoFuture, net::SocketAddr, time::SystemTime};

    use base64::URL_SAFE_NO_PAD;
    use bytes::Bytes;
    use http::Response;
    use http_body_util::Full;
    use hyper1::service::service_fn;
    use hyper_util::rt::TokioIo;
    use rand::rngs::OsRng;
    use signature::Signer;
    use tokio::net::TcpListener;

    fn new_ec_jwk(kid: String) -> (p256::SecretKey, jose_jwk::Jwk) {
        let sk = p256::SecretKey::random(&mut OsRng);
        let pk = sk.public_key().into();
        let jwk = jose_jwk::Jwk {
            key: jose_jwk::Key::Ec(pk),
            prm: jose_jwk::Parameters {
                kid: Some(kid),
                alg: Some(jose_jwa::Algorithm::Signing(jose_jwa::Signing::Es256)),
                ..Default::default()
            },
        };
        (sk, jwk)
    }

    fn new_rsa_jwk(kid: String) -> (rsa::RsaPrivateKey, jose_jwk::Jwk) {
        let sk = rsa::RsaPrivateKey::new(&mut OsRng, 2048).unwrap();
        let pk = sk.to_public_key().into();
        let jwk = jose_jwk::Jwk {
            key: jose_jwk::Key::Rsa(pk),
            prm: jose_jwk::Parameters {
                kid: Some(kid),
                alg: Some(jose_jwa::Algorithm::Signing(jose_jwa::Signing::Rs256)),
                ..Default::default()
            },
        };
        (sk, jwk)
    }

    fn build_jwt_payload(kid: String, sig: jose_jwa::Signing) -> String {
        let header = JwtHeader {
            typ: "JWT",
            algorithm: jose_jwa::Algorithm::Signing(sig),
            key_id: Some(&kid),
        };
        let body = typed_json::json! {{
            "exp": SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() + 3600,
        }};

        let header =
            base64::encode_config(serde_json::to_string(&header).unwrap(), URL_SAFE_NO_PAD);
        let body = base64::encode_config(body.to_string(), URL_SAFE_NO_PAD);

        format!("{header}.{body}")
    }

    fn new_ec_jwt(kid: String, key: p256::SecretKey) -> String {
        use p256::ecdsa::{Signature, SigningKey};

        let payload = build_jwt_payload(kid, jose_jwa::Signing::Es256);
        let sig: Signature = SigningKey::from(key).sign(payload.as_bytes());
        let sig = base64::encode_config(sig.to_bytes(), URL_SAFE_NO_PAD);

        format!("{payload}.{sig}")
    }

    fn new_rsa_jwt(kid: String, key: rsa::RsaPrivateKey) -> String {
        use rsa::pkcs1v15::SigningKey;
        use rsa::signature::SignatureEncoding;

        let payload = build_jwt_payload(kid, jose_jwa::Signing::Rs256);
        let sig = SigningKey::<sha2::Sha256>::new(key).sign(payload.as_bytes());
        let sig = base64::encode_config(sig.to_bytes(), URL_SAFE_NO_PAD);

        format!("{payload}.{sig}")
    }

    #[tokio::test]
    async fn renew() {
        let (rs1, jwk1) = new_rsa_jwk("1".into());
        let (rs2, jwk2) = new_rsa_jwk("2".into());
        let (ec1, jwk3) = new_ec_jwk("3".into());
        let (ec2, jwk4) = new_ec_jwk("4".into());

        let jwt1 = new_rsa_jwt("1".into(), rs1);
        let jwt2 = new_rsa_jwt("2".into(), rs2);
        let jwt3 = new_ec_jwt("3".into(), ec1);
        let jwt4 = new_ec_jwt("4".into(), ec2);

        let foo_jwks = jose_jwk::JwkSet {
            keys: vec![jwk1, jwk3],
        };
        let bar_jwks = jose_jwk::JwkSet {
            keys: vec![jwk2, jwk4],
        };

        let service = service_fn(move |req| {
            let foo_jwks = foo_jwks.clone();
            let bar_jwks = bar_jwks.clone();
            async move {
                let jwks = match req.uri().path() {
                    "/foo" => &foo_jwks,
                    "/bar" => &bar_jwks,
                    _ => {
                        return Response::builder()
                            .status(404)
                            .body(Full::new(Bytes::new()));
                    }
                };
                let body = serde_json::to_vec(jwks).unwrap();
                Response::builder()
                    .status(200)
                    .body(Full::new(Bytes::from(body)))
            }
        });

        let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
        let server = hyper1::server::conn::http1::Builder::new();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (s, _) = listener.accept().await.unwrap();
                let serve = server.serve_connection(TokioIo::new(s), service.clone());
                tokio::spawn(serve.into_future());
            }
        });

        let client = reqwest::Client::new();

        #[derive(Clone)]
        struct Fetch(SocketAddr);

        impl FetchAuthRules for Fetch {
            async fn fetch_auth_rules(
                &self,
                _role_name: RoleName,
            ) -> anyhow::Result<Vec<AuthRule>> {
                Ok(vec![
                    AuthRule {
                        id: "foo".to_owned(),
                        jwks_url: format!("http://{}/foo", self.0).parse().unwrap(),
                        audience: None,
                    },
                    AuthRule {
                        id: "bar".to_owned(),
                        jwks_url: format!("http://{}/bar", self.0).parse().unwrap(),
                        audience: None,
                    },
                ])
            }
        }

        let role_name = RoleName::from("user");

        let jwk_cache = Arc::new(JwkCacheEntryLock::default());

        for token in [jwt1, jwt2, jwt3, jwt4] {
            jwk_cache
                .check_jwt(
                    &RequestMonitoring::test(),
                    &token,
                    &client,
                    role_name.clone(),
                    &Fetch(addr),
                )
                .await
                .unwrap();
        }
    }
}
