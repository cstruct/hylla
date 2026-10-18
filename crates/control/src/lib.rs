//! hylla's control plane UI: the status of each backend, the mappings, and browsing the
//! objects of a mapping, changing them where the mapping is writable.
//!
//! The UI is rendered on the server, which provides a [`ControlPlane`] to its server
//! functions, and works without JavaScript: changes are forms posted to the server. With
//! the `hydrate` bundle loaded, it's hydrated in the browser too.

mod app;

pub use app::{App, shell};

use serde::{Deserialize, Serialize};

/// A backend and its last requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackendInfo {
    pub name: String,
    /// Its type, such as `s3`.
    pub kind: String,
    /// The last request to it, if any since startup.
    pub last: Option<CallInfo>,
    /// The last request to it that failed, if any since startup.
    pub last_error: Option<CallInfo>,
}

/// A request to a backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallInfo {
    /// When it was made, in milliseconds since the Unix epoch.
    pub at_millis: u64,
    /// Such as `get`, `head` or `list`.
    pub operation: String,
    /// `ok`, `not_found`, `precondition_failed`, `not_modified` or `error`.
    pub outcome: String,
    pub duration_millis: u64,
    /// What went wrong, for errors.
    pub error: Option<String>,
}

/// A mapping of a server to a backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MappingInfo {
    pub server: String,
    /// Its index in the server's mappings.
    pub index: usize,
    pub path_prefix: String,
    pub backend: String,
    /// Whether its objects can be changed through the control plane.
    pub writable: bool,
}

/// The objects and folders directly under a prefix of a mapping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Listing {
    pub mapping: MappingInfo,
    /// The prefix listed, relative to the mapping, empty or ending with `/`.
    pub prefix: String,
    /// The folders under it, as full relative prefixes ending with `/`.
    pub folders: Vec<String>,
    pub objects: Vec<ObjectEntry>,
}

/// An object of a mapping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectEntry {
    /// Its path relative to the mapping.
    pub path: String,
    pub size: u64,
    pub etag: Option<String>,
    pub last_modified_millis: i64,
}

/// What the control plane does, as the server implements it for the UI's server functions.
/// Paths are relative to their mapping, which they can't leave.
#[cfg(feature = "ssr")]
#[async_trait::async_trait]
pub trait ControlPlane: Send + Sync + 'static {
    fn backends(&self) -> Vec<BackendInfo>;
    fn mappings(&self) -> Vec<MappingInfo>;
    async fn list(&self, server: &str, index: usize, prefix: &str) -> Result<Listing, String>;
    async fn delete(&self, server: &str, index: usize, path: &str) -> Result<(), String>;
    async fn rename(&self, server: &str, index: usize, from: &str, to: &str) -> Result<(), String>;
}

/// Hydrates the server-rendered UI in the browser.
#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_body(App);
}
