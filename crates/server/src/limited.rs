//! Reading response bodies from other nodes and identity providers, up to a size, so one
//! that answers with gigabytes can't exhaust memory.

use anyhow::bail;
use bytes::{Bytes, BytesMut};

/// The most a JSON answer from an identity provider may be: discovery documents, keys,
/// tokens and user info are a few KiB.
pub const JSON: usize = 1024 * 1024;

// r[impl cluster.response-limits]
// r[impl auth.provider-limits]
/// The body of `response`, failing once it's more than `max` bytes, without reading further.
pub async fn read(mut response: reqwest::Response, max: usize) -> anyhow::Result<Bytes> {
    let url = response.url().clone();
    if response
        .content_length()
        .is_some_and(|length| length > max as u64)
    {
        bail!("{url} answered with more than {max} bytes");
    }
    let mut body = BytesMut::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > max {
            bail!("{url} answered with more than {max} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

#[cfg(test)]
mod tests {
    use axum::{Router, body::Body, routing::get};
    use futures::stream;

    use super::read;

    // r[verify cluster.response-limits]
    // r[verify auth.provider-limits]
    #[tokio::test]
    async fn bodies_beyond_the_limit_are_refused() {
        let router = Router::new()
            .route("/small", get(|| async { "small" }))
            .route("/large", get(|| async { vec![0u8; 4096] }))
            // Without a Content-Length, as a chunked answer has none.
            .route(
                "/streamed",
                get(|| async {
                    let chunks = (0..4).map(|_| Ok::<_, std::io::Error>(vec![0u8; 1024]));
                    Body::from_stream(stream::iter(chunks))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let client = reqwest::Client::new();
        let get = |path: &str| client.get(format!("http://{addr}{path}")).send();

        assert_eq!(
            read(get("/small").await.unwrap(), 1000).await.unwrap(),
            "small"
        );
        assert!(read(get("/large").await.unwrap(), 1000).await.is_err());
        assert!(read(get("/streamed").await.unwrap(), 1000).await.is_err());
        assert_eq!(
            read(get("/streamed").await.unwrap(), 4096)
                .await
                .unwrap()
                .len(),
            4096
        );
        server.abort();
    }
}
