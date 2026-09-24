//! Framework-independent middleware building blocks, called after authentication.
use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Full;
use portway_core::{Receiver, ReceiverConfig, dict::DictionaryScope};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let receiver = Receiver::new(ReceiverConfig::default())?;
    // Your authentication middleware established this identity before decoding.
    let mut request = Request::builder()
        .method("POST")
        .uri("/items")
        .body(Full::new(Bytes::from_static(
            b"already authenticated request",
        )))?;
    request
        .extensions_mut()
        .insert(DictionaryScope::new(b"authenticated-user-id"));
    let decoded = receiver.decode(request).await?;
    let (request, acknowledgement) = decoded.into_parts();
    // Invoke your own handler here. Its response body type is unconstrained.
    let application_response = Response::new(request.into_body());
    let response = acknowledgement.finish(application_response);
    assert_eq!(response.status(), 200);
    Ok(())
}
