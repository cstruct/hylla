//! The status of each backend, from its last requests, for the control plane.

use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::{Duration, SystemTime},
};

/// A request to a backend.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub at: SystemTime,
    pub operation: &'static str,
    /// As the metrics name it, such as `ok` or `error`.
    pub outcome: &'static str,
    pub duration: Duration,
    /// What went wrong, for errors.
    pub error: Option<String>,
}

/// The last requests to a backend.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    pub last: Option<Call>,
    pub last_error: Option<Call>,
}

static STATUS: LazyLock<Mutex<HashMap<String, Status>>> = LazyLock::new(Mutex::default);

// r[impl control.backends]
/// Records a request to `backend`.
pub fn record(backend: &str, call: Call) {
    let mut statuses = STATUS.lock().expect("not poisoned");
    let status = statuses.entry(backend.to_string()).or_default();
    if call.outcome == "error" {
        status.last_error = Some(call.clone());
    }
    status.last = Some(call);
}

/// The last requests to `backend`, if any since startup.
pub fn of(backend: &str) -> Status {
    STATUS
        .lock()
        .expect("not poisoned")
        .get(backend)
        .cloned()
        .unwrap_or_default()
}
