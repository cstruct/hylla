use std::ops::Range;

/// The part of an object a request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestedRange {
    /// The whole object.
    Full,
    /// A byte range within the object.
    Partial(Range<u64>),
    /// A range the object can't satisfy, answered with 416.
    Unsatisfiable,
}

// r[impl server.range]
/// Parses a `Range` header for an object of `size` bytes.
///
/// Supports a single byte range, as `bytes=a-b`, `bytes=a-` or `bytes=-n`. Headers in other
/// forms, such as several ranges, are ignored and the whole object is served, as HTTP
/// allows.
pub fn parse(header: Option<&str>, size: u64) -> RequestedRange {
    let Some(spec) = header.and_then(|header| header.trim().strip_prefix("bytes=")) else {
        return RequestedRange::Full;
    };
    if spec.contains(',') {
        return RequestedRange::Full;
    }
    let Some((start, end)) = spec.trim().split_once('-') else {
        return RequestedRange::Full;
    };
    let range = match (start.trim(), end.trim()) {
        ("", "") => return RequestedRange::Full,
        // The last `n` bytes.
        ("", suffix) => {
            let Ok(suffix) = suffix.parse::<u64>() else {
                return RequestedRange::Full;
            };
            if suffix == 0 {
                return RequestedRange::Unsatisfiable;
            }
            size.saturating_sub(suffix)..size
        }
        (start, end) => {
            let Ok(start) = start.parse::<u64>() else {
                return RequestedRange::Full;
            };
            let end = match end {
                "" => size,
                end => match end.parse::<u64>() {
                    Ok(end) if end >= start => (end + 1).min(size),
                    _ => return RequestedRange::Full,
                },
            };
            start..end
        }
    };
    if range.start >= size {
        return RequestedRange::Unsatisfiable;
    }
    RequestedRange::Partial(range)
}

#[cfg(test)]
mod tests {
    use super::{RequestedRange, parse};

    // r[verify server.range]
    #[test]
    fn parses_single_byte_ranges() {
        use RequestedRange::{Full, Partial, Unsatisfiable};
        assert_eq!(parse(None, 100), Full);
        assert_eq!(parse(Some("bytes=0-9"), 100), Partial(0..10));
        assert_eq!(parse(Some("bytes=90-"), 100), Partial(90..100));
        assert_eq!(parse(Some("bytes=-10"), 100), Partial(90..100));
        assert_eq!(parse(Some("bytes=-1000"), 100), Partial(0..100));
        // Ends past the object are clamped.
        assert_eq!(parse(Some("bytes=50-1000"), 100), Partial(50..100));
        assert_eq!(parse(Some("bytes=100-"), 100), Unsatisfiable);
        assert_eq!(parse(Some("bytes=-0"), 100), Unsatisfiable);
        // Unsupported or malformed ranges serve the whole object.
        assert_eq!(parse(Some("bytes=0-1,5-6"), 100), Full);
        assert_eq!(parse(Some("bytes=9-0"), 100), Full);
        assert_eq!(parse(Some("items=0-1"), 100), Full);
        assert_eq!(parse(Some("bytes=a-b"), 100), Full);
    }
}
