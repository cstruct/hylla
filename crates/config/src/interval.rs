use std::{fmt, time::Duration};

use facet::Facet;

// r[impl config.durations]
/// A duration written like `90s`, `15m` or `1h 30m`, as parsed by
/// [humantime](https://docs.rs/humantime).
#[derive(Facet, Debug, Clone, Copy, PartialEq, Eq)]
#[facet(proxy = String)]
pub struct Interval(#[facet(opaque)] pub Duration);

impl Interval {
    pub const fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }

    pub fn duration(&self) -> Duration {
        self.0
    }
}

impl TryFrom<String> for Interval {
    type Error = humantime::DurationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        humantime::parse_duration(&value).map(Self)
    }
}

impl TryFrom<&Interval> for String {
    type Error = std::convert::Infallible;

    fn try_from(value: &Interval) -> Result<Self, Self::Error> {
        Ok(humantime::format_duration(value.0).to_string())
    }
}

impl fmt::Display for Interval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        humantime::format_duration(self.0).fmt(f)
    }
}
