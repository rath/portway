//! Which upstream a request goes to.
//!
//! Three selectors, tried in this order:
//!
//! - **Mounts.** A named upstream answers under `/<name>/`: the segment is
//!   removed and the rest of the path goes upstream unchanged. This is how a
//!   vendor's own client reaches that vendor's API through one listener, with
//!   nothing but its base URL set — bodies are not read to choose, so a
//!   bodiless catalog or health request routes like any other.
//! - **Single upstream.** Everything goes to the one upstream, as is.
//! - **Models.** Outside every mount, the JSON `model` field picks the
//!   upstream registered under that name, for a client that treats the
//!   listener as one provider serving many models.
use crate::relay::{OutBody, json_response};
use crate::{config::ForwarderConfig, forwarder::Forwarder, pool::Upstream, telemetry::Telemetry};
use bytes::Bytes;
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body::Body;
use serde::Deserialize;
use std::sync::Arc;

pub const STATS_PATH: &str = "/__portway/stats";
pub const HEALTH_PATH: &str = "/__portway/health";
pub const CAPABILITIES_PATH: &str = "/__portway/capabilities";

/// The first segment of every management path, on every hop.
const RESERVED_SEGMENT: &str = "__portway";
/// The route name of the one upstream in single-upstream mode.
pub const SINGLE_ROUTE: &str = "upstream";

pub struct Router {
    /// Every forwarder, mounts first: what stats, negotiation and reload see.
    routes: Vec<(String, Arc<Forwarder>)>,
    /// `/<name>/...` goes to this forwarder as `/...`.
    mounts: Vec<(String, Arc<Forwarder>)>,
    /// Chosen by the JSON `model` of a request outside every mount.
    models: Vec<(String, Arc<Forwarder>)>,
    /// Single-upstream mode: the one place everything goes, path untouched.
    root: Option<Arc<Forwarder>>,
    config: ForwarderConfig,
}
impl Router {
    /// `mounts` answer under `/<name>/`; `models` are chosen by the JSON
    /// `model` field at the root. Either list may be empty, not both.
    pub fn build(
        config: &ForwarderConfig,
        mounts: &[(String, String)],
        models: &[(String, String)],
        tls: Option<Arc<rustls::ClientConfig>>,
    ) -> Result<Arc<Self>, String> {
        config.validate()?;
        if mounts.is_empty() && models.is_empty() {
            return Err("at least one upstream is required".into());
        }
        let mut routes: Vec<(String, Arc<Forwarder>)> =
            Vec::with_capacity(mounts.len() + models.len());
        let mut route = |name: &str, url: &str| -> Result<Arc<Forwarder>, String> {
            if name.trim().is_empty() || routes.iter().any(|(taken, _)| taken == name) {
                return Err("route names must be nonempty and unique".into());
            }
            let upstream = Arc::new(Upstream::with_telemetry(
                url,
                tls.clone(),
                Arc::clone(&config.telemetry),
            )?);
            let forwarder = Arc::new(Forwarder::new(name, upstream, config));
            routes.push((name.to_owned(), Arc::clone(&forwarder)));
            Ok(forwarder)
        };
        let mut mounted = Vec::with_capacity(mounts.len());
        for (name, url) in mounts {
            mount_name(name, !models.is_empty())?;
            mounted.push((name.clone(), route(name, url)?));
        }
        let mut by_model = Vec::with_capacity(models.len());
        for (name, url) in models {
            by_model.push((name.clone(), route(name, url)?));
        }
        Ok(Arc::new(Self {
            routes,
            mounts: mounted,
            models: by_model,
            root: None,
            config: config.clone(),
        }))
    }
    pub fn single(
        config: &ForwarderConfig,
        url: &str,
        tls: Option<Arc<rustls::ClientConfig>>,
    ) -> Result<Arc<Self>, String> {
        config.validate()?;
        let upstream = Arc::new(Upstream::with_telemetry(
            url,
            tls,
            Arc::clone(&config.telemetry),
        )?);
        let forwarder = Arc::new(Forwarder::new(SINGLE_ROUTE, upstream, config));
        Ok(Arc::new(Self {
            routes: vec![(SINGLE_ROUTE.to_owned(), Arc::clone(&forwarder))],
            mounts: Vec::new(),
            models: Vec::new(),
            root: Some(forwarder),
            config: config.clone(),
        }))
    }
    pub fn telemetry(&self) -> &Arc<Telemetry> {
        &self.config.telemetry
    }
    pub async fn negotiate_all(&self) {
        let mut probes = tokio::task::JoinSet::new();
        for (_, fwd) in &self.routes {
            let fwd = Arc::clone(fwd);
            probes.spawn(async move { fwd.negotiate().await });
        }
        while probes.join_next().await.is_some() {}
    }
    /// Preserve compatible origin learning without sharing pools or changing active requests.
    pub fn inherit_origin_state(&self, previous: &Self) {
        for (name, next) in &self.routes {
            if let Some(old) = previous.route(name) {
                next.inherit_origin_state(old);
            }
        }
    }
    /// Every upstream, by route name: the mounts, then the models, or the one
    /// upstream (named `upstream`) in single-upstream mode.
    pub fn routes(&self) -> &[(String, Arc<Forwarder>)] {
        &self.routes
    }
    fn route(&self, name: &str) -> Option<&Arc<Forwarder>> {
        self.routes
            .iter()
            .find(|(taken, _)| taken == name)
            .map(|(_, f)| f)
    }
    fn model_names(&self) -> Vec<&str> {
        self.models.iter().map(|(name, _)| name.as_str()).collect()
    }
    /// The mount the first path segment names, with the path and the
    /// path-and-query as they go upstream: that segment removed, the query
    /// kept. `/codex/models?x=1` reaches the `codex` mount as `/models?x=1`
    /// and `/codex` as `/`.
    fn mount(&self, path: &str, path_and_query: &str) -> Option<(&Arc<Forwarder>, String, String)> {
        let segment = path.strip_prefix('/')?.split('/').next()?;
        let forwarder = self
            .mounts
            .iter()
            .find(|(name, _)| name == segment)
            .map(|(_, f)| f)?;
        let cut = 1 + segment.len();
        let rest = match &path[cut..] {
            "" => "/".to_owned(),
            rest => rest.to_owned(),
        };
        let rest_and_query = match path_and_query.get(cut..).unwrap_or("") {
            "" => "/".to_owned(),
            tail if tail.starts_with('?') => format!("/{tail}"),
            tail => tail.to_owned(),
        };
        Some((forwarder, rest, rest_and_query))
    }

    pub async fn handle<B>(self: Arc<Self>, request: Request<B>) -> Response<OutBody>
    where
        B: Body<Data = Bytes> + Unpin,
        B::Error: std::fmt::Display,
    {
        let (parts, incoming) = request.into_parts();
        let path = parts.uri.path().to_owned();
        let path_and_query = parts
            .uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
            .to_owned();
        if parts.method == Method::GET && path == HEALTH_PATH {
            return json_response(
                StatusCode::OK,
                serde_json::json!({"status":"ok","mode":if self.root.is_some() {"forward"} else {"router"}}),
            );
        }
        if parts.method == Method::GET && path == STATS_PATH {
            let upstreams: serde_json::Map<String, serde_json::Value> = self
                .routes
                .iter()
                .map(|(n, f)| (n.clone(), f.snapshot()))
                .collect();
            return json_response(StatusCode::OK, serde_json::json!({"upstreams":upstreams}));
        }
        if parts.method == Method::CONNECT || parts.headers.contains_key(http::header::UPGRADE) {
            return invalid_request(
                StatusCode::NOT_IMPLEMENTED,
                "Tunnels and protocol upgrades are not supported.",
                None,
            );
        }
        let scope = parts
            .extensions
            .get::<crate::dict::DictionaryScope>()
            .cloned()
            .unwrap_or_else(|| crate::dict::DictionaryScope::from_headers(&parts.headers));
        // A mount is chosen by the path alone, so the body goes upstream as
        // it came, encoded or not; it is read only to record which model the
        // request named.
        if let Some((forwarder, rest, rest_and_query)) = self.mount(&path, &path_and_query) {
            let body = match crate::body::collect_raw(incoming, self.config.max_body_bytes).await {
                Ok(body) => body,
                Err(err) => return invalid_request(err.status(), &err.to_string(), None),
            };
            let named = named(&parts.headers, &body);
            return forwarder
                .handle_scoped(
                    parts.method,
                    rest_and_query,
                    rest,
                    parts.headers,
                    body,
                    scope,
                    named,
                )
                .await;
        }
        if let Some(root) = &self.root {
            let body = match crate::body::collect_raw(incoming, self.config.max_body_bytes).await {
                Ok(body) => body,
                Err(err) => return invalid_request(err.status(), &err.to_string(), None),
            };
            let named = named(&parts.headers, &body);
            return root
                .handle_scoped(
                    parts.method,
                    path_and_query,
                    path,
                    parts.headers,
                    body,
                    scope,
                    named,
                )
                .await;
        }
        if self.models.is_empty() {
            // Only mounts are configured: nothing answers at the root, and
            // the message names where the upstreams are instead.
            let mut mounted: Vec<String> =
                self.mounts.iter().map(|(n, _)| format!("/{n}")).collect();
            mounted.sort();
            return invalid_request(
                StatusCode::NOT_FOUND,
                &format!(
                    "No upstream is mounted at {path:?}. Mounted: {}",
                    mounted.join(", ")
                ),
                None,
            );
        }
        if parts.method == Method::GET && path == "/v1/models" {
            let data: Vec<_> = self.model_names().into_iter().map(|id| serde_json::json!({"id":id,"object":"model","created":0,"owned_by":"configured"})).collect();
            return json_response(
                StatusCode::OK,
                serde_json::json!({"object":"list","data":data}),
            );
        }
        let encoded = parts
            .headers
            .get(http::header::CONTENT_ENCODING)
            .is_some_and(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"));
        if encoded {
            return invalid_request(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Model routing requires an uncompressed JSON body.",
                None,
            );
        }
        let body = match crate::body::collect_raw(incoming, self.config.max_body_bytes).await {
            Ok(body) => body,
            Err(err) => return invalid_request(err.status(), &err.to_string(), None),
        };
        let mut model = query_model(parts.uri.query());
        let mut service_tier = None;
        if !body.is_empty() {
            let Ok(serde_json::Value::Object(mut payload)) =
                serde_json::from_slice::<serde_json::Value>(&body)
            else {
                return invalid_request(StatusCode::BAD_REQUEST, "Expected a JSON object.", None);
            };
            model = string(payload.remove("model"));
            service_tier = string(payload.remove("service_tier"));
        }
        let Some(forwarder) = model
            .as_deref()
            .and_then(|m| self.models.iter().find(|(name, _)| name == m))
            .map(|(_, f)| f)
        else {
            // Naming the requested model — or its absence — makes the 400
            // actionable without re-reading the request.
            let message = match model.as_deref() {
                Some(requested) => format!(
                    "Unknown model {requested:?}. Specify a supported model: {}",
                    self.model_names().join(", ")
                ),
                None => format!(
                    "Specify a supported model: {}",
                    self.model_names().join(", ")
                ),
            };
            return invalid_request(StatusCode::BAD_REQUEST, &message, Some("model"));
        };
        forwarder
            .handle_scoped(
                parts.method,
                path_and_query,
                path,
                parts.headers,
                body,
                scope,
                Named {
                    model,
                    service_tier,
                },
            )
            .await
    }
}

/// Only the recorded fields are kept; every other value is skipped as it is
/// read.
#[derive(Deserialize)]
struct NamedFields {
    #[serde(default)]
    model: Option<serde_json::Value>,
    #[serde(default)]
    service_tier: Option<serde_json::Value>,
}

/// What a request names in its JSON body and is recorded under. Neither
/// field has any part in where it goes; routing has its own rules.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Named {
    /// The string `model`. Prices are keyed by it.
    pub model: Option<String>,
    /// The string `service_tier`: the speed or priority class a vendor bills
    /// at rates of its own. A vendor's standard class is usually its absence.
    pub service_tier: Option<String>,
}

/// What a request names: the string `model` and `service_tier` of its JSON
/// object body, read in one pass. Empty for a body that is encoded or is not
/// a JSON object, and each field `None` where the body names no string.
pub fn named(headers: &HeaderMap, body: &[u8]) -> Named {
    let encoded = headers
        .get(http::header::CONTENT_ENCODING)
        .is_some_and(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"));
    if encoded || body.is_empty() {
        return Named::default();
    }
    let Ok(fields) = serde_json::from_slice::<NamedFields>(body) else {
        return Named::default();
    };
    Named {
        model: string(fields.model),
        service_tier: string(fields.service_tier),
    }
}

fn string(value: Option<serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(text) => Some(text),
        _ => None,
    }
}

/// A mount is one path segment, so a client's base URL can end in it and
/// the proxy's own paths cannot collide with it.
fn mount_name(name: &str, with_models: bool) -> Result<(), String> {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~');
    if name.is_empty() || name == "." || name == ".." || !name.chars().all(plain) {
        return Err(format!(
            "upstream name {name:?} is not a path segment: use letters, digits, '-', '.', '_' or '~'"
        ));
    }
    if name == RESERVED_SEGMENT {
        return Err(format!(
            "upstream name {name:?} is reserved for management paths"
        ));
    }
    if with_models && name == "v1" {
        return Err(
            "an upstream named \"v1\" would hide the /v1 paths that [models] routes".into(),
        );
    }
    Ok(())
}

fn query_model(query: Option<&str>) -> Option<String> {
    let query = query?;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "model").then(|| percent_decode(value))
    })
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|s| u8::from_str_radix(s, 16).ok())
                .ok_or(())
            {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn invalid_request(status: StatusCode, message: &str, param: Option<&str>) -> Response<OutBody> {
    json_response(
        status,
        serde_json::json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "param": param,
            }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router(mounts: &[&str], models: &[&str]) -> Result<Arc<Router>, String> {
        let pair = |name: &&str| ((*name).to_owned(), "http://example.test".to_owned());
        Router::build(
            &ForwarderConfig::default(),
            &mounts.iter().map(pair).collect::<Vec<_>>(),
            &models.iter().map(pair).collect::<Vec<_>>(),
            None,
        )
    }

    #[test]
    fn a_mount_takes_its_segment_and_leaves_the_query() {
        let router = router(&["codex", "anthropic"], &[]).unwrap();
        let cases = [
            (
                "/codex/models",
                "/codex/models?client_version=1",
                "/models",
                "/models?client_version=1",
            ),
            ("/codex", "/codex", "/", "/"),
            ("/codex", "/codex?x=1", "/", "/?x=1"),
            ("/codex/", "/codex/", "/", "/"),
            (
                "/anthropic/v1/messages",
                "/anthropic/v1/messages",
                "/v1/messages",
                "/v1/messages",
            ),
        ];
        for (path, path_and_query, rest, rest_and_query) in cases {
            let (forwarder, got, got_and_query) = router.mount(path, path_and_query).unwrap();
            assert_eq!(forwarder.name, path[1..].split('/').next().unwrap());
            assert_eq!(
                (got.as_str(), got_and_query.as_str()),
                (rest, rest_and_query),
                "{path}"
            );
        }
        for path in [
            "/",
            "/v1/messages",
            "/codexx/models",
            "/Codex/models",
            "/__portway/stats",
        ] {
            assert!(router.mount(path, path).is_none(), "{path}");
        }
    }

    #[test]
    fn mount_names_are_single_segments_outside_the_reserved_ones() {
        for name in ["anthropic", "my-api", "v2", "a.b_c~"] {
            assert!(router(&[name], &[]).is_ok(), "{name}");
        }
        for name in ["", "a/b", "a b", "a?b", ".", "..", "__portway", "a%20b"] {
            assert!(router(&[name], &[]).is_err(), "{name:?}");
        }
        assert!(router(&["v1"], &[]).is_ok());
        assert!(router(&["v1"], &["model-a"]).is_err());
        assert!(router(&["same"], &["same"]).is_err());
        assert!(router(&[], &[]).is_err());
    }

    #[test]
    fn the_named_fields_are_the_strings_of_a_plain_json_object() {
        let plain = HeaderMap::new();
        let mut encoded = HeaderMap::new();
        encoded.insert("content-encoding", "zstd".parse().unwrap());
        let mut identity = HeaderMap::new();
        identity.insert("content-encoding", "identity".parse().unwrap());
        let body = br#"{"messages":[{"role":"user","content":"hi"}],"model":"m-1","service_tier":"tier-a","stream":true}"#;
        let both = Named {
            model: Some("m-1".into()),
            service_tier: Some("tier-a".into()),
        };
        assert_eq!(named(&plain, body), both);
        assert_eq!(named(&identity, body), both);
        assert_eq!(named(&encoded, body), Named::default());
        for body in [
            &b""[..],
            b"{}",
            b"[]",
            b"null",
            b"{",
            br#"{"model":5,"service_tier":5}"#,
            br#"{"model":null,"service_tier":null}"#,
        ] {
            assert_eq!(named(&plain, body), Named::default(), "{body:?}");
        }
        // Each is read on its own: a standard-class request names no tier.
        assert_eq!(
            named(&plain, br#"{"model":"m-1"}"#),
            Named {
                model: Some("m-1".into()),
                service_tier: None,
            }
        );
    }

    #[test]
    fn routes_list_mounts_first_and_single_mode_names_its_one_upstream() {
        let router = router(&["codex"], &["model-a"]).unwrap();
        let names: Vec<&str> = router.routes().iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["codex", "model-a"]);
        let single =
            Router::single(&ForwarderConfig::default(), "http://example.test", None).unwrap();
        assert_eq!(single.routes()[0].0, SINGLE_ROUTE);
        assert!(single.root.is_some());
    }
}
