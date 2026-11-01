//! Another hylla node as a backend, reading the backends it serves over mutual TLS.

use std::{fmt, ops::Range};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::{StreamExt, TryStreamExt, stream, stream::BoxStream};
use http::{StatusCode, header};
use hylla_config::HyllaTls;
use object_store::{
    Attribute, Attributes, CopyOptions, GetOptions, GetRange, GetResult, GetResultPayload,
    ListResult, MultipartUpload, ObjectMeta, ObjectStore, PutMultipartOptions, PutOptions,
    PutPayload, PutResult, Result, path::Path,
};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::{spans, upstream::Listing};

/// What object_store errors name this store as.
const STORE: &str = "Hylla";

/// Characters escaped in URL path segments: all but unreserved ones.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

// r[impl backend.hylla]
/// A backend another hylla node serves.
#[derive(Debug)]
pub struct HyllaStore {
    /// The node's base URL, without a trailing slash.
    url: String,
    /// The backend's name on that node.
    backend: String,
    client: reqwest::Client,
}

impl HyllaStore {
    // r[impl upstream.tls]
    pub fn new(url: &str, backend: &str, tls: &HyllaTls) -> anyhow::Result<Self> {
        let files = crate::tls::Files {
            ca: &tls.ca,
            cert: &tls.cert,
            key: &tls.key,
        };
        // The node is checked for its URL's host, as HTTPS servers are.
        let client = reqwest::Client::builder()
            .use_preconfigured_tls(crate::tls::client_config(&files, None)?)
            .build()?;
        Ok(Self {
            url: url.trim_end_matches('/').to_string(),
            backend: backend.to_string(),
            client,
        })
    }

    fn backend_url(&self) -> String {
        format!(
            "{}/backends/{}",
            self.url,
            utf8_percent_encode(&self.backend, SEGMENT)
        )
    }

    /// The URL of an object, its path in its encoded form, so the other node reads the same
    /// object.
    fn object_url(&self, location: &Path) -> String {
        let key = location
            .as_ref()
            .split('/')
            .map(|segment| utf8_percent_encode(segment, SEGMENT).to_string())
            .collect::<Vec<_>>()
            .join("/");
        format!("{}/objects/{key}", self.backend_url())
    }
}

impl fmt::Display for HyllaStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hylla({}, {})", self.url, self.backend)
    }
}

fn generic(error: impl std::error::Error + Send + Sync + 'static) -> object_store::Error {
    object_store::Error::Generic {
        store: STORE,
        source: Box::new(error),
    }
}

fn generic_message(message: String) -> object_store::Error {
    object_store::Error::Generic {
        store: STORE,
        source: message.into(),
    }
}

// r[impl backend.hylla.read-only]
fn read_only() -> object_store::Error {
    object_store::Error::NotSupported {
        source: "hylla backends are read only".into(),
    }
}

fn range_header(range: &GetRange) -> String {
    match range {
        GetRange::Bounded(range) => format!("bytes={}-{}", range.start, range.end - 1),
        GetRange::Offset(start) => format!("bytes={start}-"),
        GetRange::Suffix(len) => format!("bytes=-{len}"),
    }
}

/// The range and the object's size of a `Content-Range` such as `bytes 0-99/1000`.
fn content_range(value: &str) -> Option<(Range<u64>, u64)> {
    let (range, size) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, size) = (
        start.parse::<u64>().ok()?,
        end.parse::<u64>().ok()?,
        size.parse().ok()?,
    );
    Some((start..end + 1, size))
}

#[async_trait]
impl ObjectStore for HyllaStore {
    // r[impl backend.hylla.reads]
    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        if options.if_modified_since.is_some()
            || options.if_unmodified_since.is_some()
            || options.version.is_some()
        {
            return Err(object_store::Error::NotSupported {
                source: "hylla backends only take ETag preconditions".into(),
            });
        }
        let url = self.object_url(location);
        let mut request = if options.head {
            self.client.head(url)
        } else {
            self.client.get(url)
        };
        if let Some(range) = &options.range {
            request = request.header(header::RANGE, range_header(range));
        }
        if let Some(etag) = &options.if_match {
            request = request.header(header::IF_MATCH, etag);
        }
        if let Some(etag) = &options.if_none_match {
            request = request.header(header::IF_NONE_MATCH, etag);
        }
        let response = request
            .headers(spans::trace_headers())
            .send()
            .await
            .map_err(generic)?;

        let path = location.to_string();
        match response.status() {
            StatusCode::OK | StatusCode::PARTIAL_CONTENT => {}
            StatusCode::NOT_FOUND => {
                return Err(object_store::Error::NotFound {
                    path,
                    source: format!("{self} has no such object").into(),
                });
            }
            StatusCode::PRECONDITION_FAILED => {
                return Err(object_store::Error::Precondition {
                    path,
                    source: "the object's ETag doesn't match".into(),
                });
            }
            StatusCode::NOT_MODIFIED => {
                return Err(object_store::Error::NotModified {
                    path,
                    source: "the object's ETag matches".into(),
                });
            }
            status => {
                let body = response.text().await.unwrap_or_default();
                return Err(generic_message(format!(
                    "{self} answered {status}: {}",
                    body.trim()
                )));
            }
        }

        let headers = response.headers();
        let text =
            |name: header::HeaderName| headers.get(name).and_then(|value| value.to_str().ok());
        let (range, size) = match text(header::CONTENT_RANGE).and_then(content_range) {
            Some(range_and_size) => range_and_size,
            None => {
                let size = text(header::CONTENT_LENGTH)
                    .and_then(|length| length.parse::<u64>().ok())
                    .ok_or_else(|| generic_message(format!("{self} sent no Content-Length")))?;
                (0..size, size)
            }
        };
        let meta = ObjectMeta {
            location: location.clone(),
            last_modified: text(header::LAST_MODIFIED)
                .and_then(|date| httpdate::parse_http_date(date).ok())
                .map(DateTime::<Utc>::from)
                .unwrap_or_default(),
            size,
            e_tag: text(header::ETAG).map(str::to_string),
            version: None,
        };
        let mut attributes = Attributes::new();
        if let Some(content_type) = text(header::CONTENT_TYPE) {
            attributes.insert(Attribute::ContentType, content_type.to_string().into());
        }
        let payload = if options.head {
            stream::empty().boxed()
        } else {
            response.bytes_stream().map_err(generic).boxed()
        };
        Ok(GetResult {
            payload: GetResultPayload::Stream(payload),
            meta,
            range,
            attributes,
            extensions: Default::default(),
        })
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        let query = serde_urlencoded::to_string([(
            "prefix",
            prefix.map(ToString::to_string).unwrap_or_default(),
        )])
        .expect("a string pair encodes");
        let request = self
            .client
            .get(format!("{}/list?{query}", self.backend_url()))
            .headers(spans::trace_headers());
        let name = self.to_string();
        stream::once(async move {
            let response = request.send().await.map_err(generic)?;
            let status = response.status();
            let body: Bytes = response.bytes().await.map_err(generic)?;
            if !status.is_success() {
                let body = String::from_utf8_lossy(&body);
                return Err(generic_message(format!(
                    "{name} answered {status}: {}",
                    body.trim()
                )));
            }
            let listing = serde_json::from_slice::<Listing>(&body).map_err(generic)?;
            let objects = listing.objects.into_iter().map(|listed| {
                Ok(ObjectMeta {
                    location: Path::parse(&listed.key)
                        .unwrap_or_else(|_| Path::from(listed.key.as_str())),
                    last_modified: DateTime::from_timestamp_millis(listed.last_modified_millis)
                        .unwrap_or_default(),
                    size: listed.size,
                    e_tag: listed.etag,
                    version: None,
                })
            });
            Ok(stream::iter(objects))
        })
        .try_flatten()
        .boxed()
    }

    async fn list_with_delimiter(&self, _prefix: Option<&Path>) -> Result<ListResult> {
        Err(object_store::Error::NotSupported {
            source: "hylla backends only list without a delimiter".into(),
        })
    }

    async fn put_opts(
        &self,
        _location: &Path,
        _payload: PutPayload,
        _opts: PutOptions,
    ) -> Result<PutResult> {
        Err(read_only())
    }

    async fn put_multipart_opts(
        &self,
        _location: &Path,
        _opts: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        Err(read_only())
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        locations.map(|_| Err(read_only())).boxed()
    }

    async fn copy_opts(&self, _from: &Path, _to: &Path, _options: CopyOptions) -> Result<()> {
        Err(read_only())
    }
}

#[cfg(test)]
mod tests {
    use object_store::GetRange;

    use super::{content_range, range_header};

    #[test]
    fn ranges_translate_to_and_from_headers() {
        assert_eq!(range_header(&GetRange::Bounded(0..100)), "bytes=0-99");
        assert_eq!(range_header(&GetRange::Offset(10)), "bytes=10-");
        assert_eq!(range_header(&GetRange::Suffix(5)), "bytes=-5");
        assert_eq!(content_range("bytes 0-99/1000"), Some((0..100, 1000)));
        assert_eq!(content_range("bytes */1000"), None);
    }
}
