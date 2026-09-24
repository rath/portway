//! Explicit single-upstream or OpenAI model routing.
use crate::relay::{OutBody, json_response};
use crate::{config::ForwarderConfig, forwarder::Forwarder, pool::Upstream, telemetry::Telemetry};
use bytes::Bytes;
use http::{Method, Request, Response, StatusCode};
use http_body::Body;
use std::sync::Arc;

pub const STATS_PATH: &str = "/__portway/stats";
pub const HEALTH_PATH: &str = "/__portway/health";
pub const CAPABILITIES_PATH: &str = "/__portway/capabilities";

pub struct Router {
    models: Vec<(String, Arc<Forwarder>)>,
    single: bool,
    config: ForwarderConfig,
}
impl Router {
    pub fn build(
        config: &ForwarderConfig,
        upstreams: &[(String, String)],
        tls: Option<Arc<rustls::ClientConfig>>,
    ) -> Result<Arc<Self>, String> {
        config.validate()?;
        if upstreams.is_empty() {
            return Err("at least one upstream is required".into());
        }
        let mut models = Vec::with_capacity(upstreams.len());
        for (model, url) in upstreams {
            if model.trim().is_empty() || models.iter().any(|(name, _)| name == model) {
                return Err("route names must be nonempty and unique".into());
            }
            let upstream = Arc::new(Upstream::with_telemetry(
                url,
                tls.clone(),
                Arc::clone(&config.telemetry),
            )?);
            models.push((
                model.clone(),
                Arc::new(Forwarder::new(model, upstream, config)),
            ));
        }
        Ok(Arc::new(Self {
            models,
            single: false,
            config: config.clone(),
        }))
    }
    pub fn single(
        config: &ForwarderConfig,
        url: &str,
        tls: Option<Arc<rustls::ClientConfig>>,
    ) -> Result<Arc<Self>, String> {
        let mut router = Self::build(config, &[("upstream".into(), url.into())], tls)?;
        Arc::get_mut(&mut router).expect("new router").single = true;
        Ok(router)
    }
    pub fn telemetry(&self) -> &Arc<Telemetry> {
        &self.config.telemetry
    }
    pub async fn negotiate_all(&self) {
        let mut probes = tokio::task::JoinSet::new();
        for (_, fwd) in &self.models {
            let fwd = Arc::clone(fwd);
            probes.spawn(async move { fwd.negotiate().await });
        }
        while probes.join_next().await.is_some() {}
    }
    pub fn models(&self) -> &[(String, Arc<Forwarder>)] {
        &self.models
    }
    fn get(&self, model: &str) -> Option<&Arc<Forwarder>> {
        self.models
            .iter()
            .find(|(name, _)| name == model)
            .map(|(_, f)| f)
    }
    fn names(&self) -> Vec<&str> {
        self.models.iter().map(|(name, _)| name.as_str()).collect()
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
                serde_json::json!({"status":"ok","mode":if self.single {"forward"} else {"router"}}),
            );
        }
        if parts.method == Method::GET && path == STATS_PATH {
            let models: serde_json::Map<String, serde_json::Value> = self
                .models
                .iter()
                .map(|(n, f)| (n.clone(), f.snapshot()))
                .collect();
            return json_response(StatusCode::OK, serde_json::json!({"models":models}));
        }
        if parts.method == Method::CONNECT || parts.headers.contains_key(http::header::UPGRADE) {
            return invalid_request(
                StatusCode::NOT_IMPLEMENTED,
                "Tunnels and protocol upgrades are not supported.",
                None,
            );
        }
        if !self.single && parts.method == Method::GET && path == "/v1/models" {
            let data: Vec<_> = self.names().into_iter().map(|id| serde_json::json!({"id":id,"object":"model","created":0,"owned_by":"configured"})).collect();
            return json_response(
                StatusCode::OK,
                serde_json::json!({"object":"list","data":data}),
            );
        }
        let encoded = parts
            .headers
            .get(http::header::CONTENT_ENCODING)
            .is_some_and(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"));
        if !self.single && encoded {
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
        let forwarder = if self.single {
            &self.models[0].1
        } else {
            let mut model = query_model(parts.uri.query());
            if !body.is_empty() {
                let Ok(serde_json::Value::Object(payload)) =
                    serde_json::from_slice::<serde_json::Value>(&body)
                else {
                    return invalid_request(
                        StatusCode::BAD_REQUEST,
                        "Expected a JSON object.",
                        None,
                    );
                };
                model = payload
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
            }
            let Some(fwd) = model.as_deref().and_then(|m| self.get(m)) else {
                return invalid_request(
                    StatusCode::BAD_REQUEST,
                    &format!("Specify a supported model: {}", self.names().join(", ")),
                    Some("model"),
                );
            };
            fwd
        };
        let scope = parts
            .extensions
            .get::<crate::dict::DictionaryScope>()
            .cloned()
            .unwrap_or_else(|| crate::dict::DictionaryScope::from_headers(&parts.headers));
        forwarder
            .handle_scoped(
                parts.method,
                path_and_query,
                path,
                parts.headers,
                body,
                scope,
            )
            .await
    }
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
