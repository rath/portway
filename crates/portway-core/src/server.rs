//! The loopback listener the agent talks to.

use std::convert::Infallible;
use std::sync::Arc;

use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::router::Router;

/// Serve until the listener dies.
///
/// `half_close` stays at hyper's default of `false` on purpose: that is what
/// makes hyper notice an agent that hung up mid-request and drop the service
/// future. Dropping it drops the upstream response body, which closes the
/// upstream HTTP/1.1 connection, which is what stops the engine generating.
pub async fn serve(listener: TcpListener, router: Arc<Router>) {
    let telemetry = Arc::clone(router.telemetry());
    serve_with(listener, telemetry, move |request| {
        Arc::clone(&router).handle(request)
    })
    .await;
}

/// A decompression sidecar in front of an ordinary single-upstream router.
pub async fn serve_receiver(
    listener: TcpListener,
    router: Arc<Router>,
    receiver: Arc<crate::Receiver>,
) {
    let telemetry = Arc::clone(router.telemetry());
    serve_with(
        listener,
        telemetry,
        move |request: http::Request<hyper::body::Incoming>| {
            let router = Arc::clone(&router);
            let receiver = Arc::clone(&receiver);
            async move {
                if request.method() == http::Method::GET
                    && request.uri().path() == crate::router::STATS_PATH
                {
                    let models: serde_json::Map<String, serde_json::Value> = router
                        .models()
                        .iter()
                        .map(|(n, f)| (n.clone(), f.snapshot()))
                        .collect();
                    return crate::relay::json_response(
                        http::StatusCode::OK,
                        serde_json::json!({"models":models,"receiver":receiver.snapshot()}),
                    );
                }
                receiver
                    .handle(request, move |request| {
                        router.handle(request.map(http_body_util::Full::new))
                    })
                    .await
            }
        },
    )
    .await;
}

/// Connections belong to this future and are cancelled when it is dropped.
pub async fn serve_with<F, Fut>(listener: TcpListener, telemetry: Arc<crate::Telemetry>, handler: F)
where
    F: Fn(http::Request<hyper::body::Incoming>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = http::Response<crate::relay::OutBody>> + Send + 'static,
{
    let handler = Arc::new(handler);
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted=listener.accept()=> {
                let (stream,_peer)=match accepted {
                    Ok(pair)=>pair,
                    Err(err)=> {telemetry.error(&format!("accept failed: {err}")); tokio::time::sleep(std::time::Duration::from_millis(100)).await; continue;}
                };
                let _=stream.set_nodelay(true); let handler=Arc::clone(&handler);
                connections.spawn(async move {
                    let service=service_fn(move |request| {let handler=Arc::clone(&handler); async move {Ok::<_,Infallible>(handler(request).await)}});
                    let _=hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream),service).await;
                });
            }
            _=connections.join_next(), if !connections.is_empty()=>{}
        }
    }
}

/// Root certificates, ALPN pinned to HTTP/1.1, one config shared by every pool
/// so rustls' session store actually resumes across re-dials.
pub fn tls_config() -> Arc<rustls::ClientConfig> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("supported TLS protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}
