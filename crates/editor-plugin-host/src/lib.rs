//! Plugin host: manifest loading, capability enforcement, WASM runtime (skeleton), crash
//! isolation (ADR-005, §65–68, Invariant 8).
//!
//! The WASM runtime (`wasmtime`) is intentionally NOT wired at MVP stage (§108 requires
//! only a plugin API skeleton). This host implements everything around it: TOML manifest
//! parsing, API-version negotiation, capability enforcement (default deny), and lifecycle
//! management where a crashed plugin is deactivated rather than fatal (Invariant 8).

#![forbid(unsafe_code)]

mod manifest;

pub use manifest::{ManifestParseError, parse_manifest};

use editor_domain::ids::PluginId;
use editor_plugin_api::{Capability, PluginManifest};

/// Typed plugin error (§89).
#[derive(Debug)]
pub enum PluginError {
    ManifestInvalid(String),
    IncompatibleApi {
        expected: String,
        got: String,
    },
    /// Another ACTIVE plugin already owns this id.
    DuplicateId(PluginId),
    PermissionDenied(Capability),
    /// The plugin crashed; it has been deactivated (Invariant 8). The editor continues.
    Crash(String),
    NotActive(PluginId),
    Other(String),
}

impl core::fmt::Display for PluginError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&match self {
            Self::ManifestInvalid(s) => format!("invalid manifest: {s}"),
            Self::IncompatibleApi { expected, got } => {
                format!("incompatible api: expected {expected}, got {got}")
            }
            Self::DuplicateId(id) => format!("plugin id already registered: {id}"),
            Self::PermissionDenied(c) => format!("permission denied: {c:?}"),
            Self::Crash(s) => format!("plugin crashed: {s}"),
            Self::NotActive(id) => format!("plugin not active: {id}"),
            Self::Other(s) => s.clone(),
        })
    }
}
impl std::error::Error for PluginError {}

/// A loaded plugin instance.
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub state: PluginState,
}

/// Lifecycle state of a plugin (Invariant 8: a crashed plugin is deactivated, not fatal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginState {
    /// Plugin is loaded and may receive host calls.
    Active,
    /// Plugin was explicitly deactivated (e.g. by the user).
    Deactivated,
    /// Plugin panicked/crashed and was force-deactivated. The editor continues.
    Crashed,
}

/// The plugin host. Loads manifests, enforces capabilities, tracks lifecycle.
pub struct PluginHost {
    plugins: Vec<LoadedPlugin>,
    /// The API version the host itself speaks (§66).
    api_version: editor_plugin_api::ApiVersion,
}

impl PluginHost {
    /// Create a host targeting SDK api_version 1.x.
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
            api_version: editor_plugin_api::ApiVersion { major: 1, minor: 0 },
        }
    }

    /// The API version this host requires.
    pub fn api_version(&self) -> &editor_plugin_api::ApiVersion {
        &self.api_version
    }

    /// Validate and register a plugin from a TOML manifest string.
    pub fn register_manifest(&mut self, toml: &str) -> Result<PluginId, PluginError> {
        let manifest =
            parse_manifest(toml).map_err(|e| PluginError::ManifestInvalid(e.to_string()))?;
        self.register(manifest)
    }

    /// Validate and register a plugin manifest. Returns the plugin id on success.
    pub fn register(&mut self, manifest: PluginManifest) -> Result<PluginId, PluginError> {
        if manifest.api_version.major != self.api_version.major {
            return Err(PluginError::IncompatibleApi {
                expected: format!("{}.x", self.api_version.major),
                got: format!(
                    "{}.{}",
                    manifest.api_version.major, manifest.api_version.minor
                ),
            });
        }
        // Reject duplicate plugin IDs to prevent capability confusion — but
        // only when the existing entry is Active: a crashed or deactivated
        // plugin must be re-registrable (e.g. after the user updates it),
        // otherwise a crash would permanently block the plugin id.
        let id = manifest.id.clone();
        if self
            .plugins
            .iter()
            .any(|p| p.manifest.id == id && p.state == PluginState::Active)
        {
            return Err(PluginError::DuplicateId(id));
        }
        self.plugins.retain(|p| p.manifest.id != id);
        self.plugins.push(LoadedPlugin {
            manifest,
            state: PluginState::Active,
        });
        Ok(id)
    }

    /// Deactivate a plugin (e.g. after a crash). The editor continues (Invariant 8).
    pub fn deactivate(&mut self, id: &PluginId, reason: PluginState) {
        if let Some(p) = self.plugins.iter_mut().find(|p| &p.manifest.id == id) {
            p.state = reason;
        }
    }

    /// Mark a plugin as crashed and deactivate it (§96). Returns `Ok(())` — the editor is
    /// unaffected (Invariant 8).
    pub fn handle_crash(&mut self, id: &PluginId, reason: &str) {
        // In a real host this would also tear down the WASM instance. For the skeleton we
        // record the crash and deactivate.
        self.deactivate(id, PluginState::Crashed);
        let _ = reason;
    }

    /// Check whether a plugin has a capability (§68). Default deny — and only
    /// *active* plugins may exercise capabilities at all: a crashed or
    /// deactivated plugin must not retain permissions (Invariant 8).
    pub fn has_capability(&self, id: &PluginId, cap: Capability) -> bool {
        self.plugins
            .iter()
            .find(|p| &p.manifest.id == id)
            .map(|p| p.state == PluginState::Active && p.manifest.permissions.contains(&cap))
            .unwrap_or(false)
    }

    /// Assert a plugin has a capability; returns `PermissionDenied` otherwise (§68).
    pub fn require_capability(&self, id: &PluginId, cap: Capability) -> Result<(), PluginError> {
        if self.has_capability(id, cap) {
            Ok(())
        } else {
            Err(PluginError::PermissionDenied(cap))
        }
    }

    /// Get the state of a plugin.
    pub fn state(&self, id: &PluginId) -> Option<PluginState> {
        self.plugins
            .iter()
            .find(|p| &p.manifest.id == id)
            .map(|p| p.state)
    }

    /// Only active plugins (excludes deactivated/crashed).
    pub fn active(&self) -> impl Iterator<Item = &LoadedPlugin> {
        self.plugins
            .iter()
            .filter(|p| p.state == PluginState::Active)
    }

    pub fn plugins(&self) -> &[LoadedPlugin] {
        &self.plugins
    }
}

impl Default for PluginHost {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_MANIFEST: &str = r#"
id = "com.example.mermaid"
name = "Mermaid"
version = "1.0.0"
api_version = "1"

[permissions]
filesystem = false
network = true
git_credentials = false
clipboard = false
process = false

[contributes]
commands = true
markdown_extensions = true
renderers = true
"#;

    #[test]
    fn register_valid_manifest() {
        let mut host = PluginHost::new();
        let id = host.register_manifest(VALID_MANIFEST).expect("register");
        assert_eq!(id.0, "com.example.mermaid");
        assert_eq!(host.state(&id), Some(PluginState::Active));
    }

    #[test]
    fn incompatible_api_rejected() {
        let mut host = PluginHost::new();
        let bad = VALID_MANIFEST.replacen("api_version = \"1\"", "api_version = \"2\"", 1);
        let err = host.register_manifest(&bad).unwrap_err();
        assert!(matches!(err, PluginError::IncompatibleApi { .. }));
    }

    #[test]
    fn capability_default_deny() {
        let mut host = PluginHost::new();
        let id = host.register_manifest(VALID_MANIFEST).expect("register");
        // network is granted.
        assert!(host.has_capability(&id, Capability::Network));
        // filesystem is NOT granted.
        assert!(!host.has_capability(&id, Capability::Filesystem));
        // git_credentials is NOT granted -> require fails.
        assert!(matches!(
            host.require_capability(&id, Capability::GitCredentials),
            Err(PluginError::PermissionDenied(_))
        ));
    }

    #[test]
    fn crash_deactivates_but_editor_continues() {
        let mut host = PluginHost::new();
        let id = host.register_manifest(VALID_MANIFEST).expect("register");
        host.handle_crash(&id, "wasm trap: unreachable");
        assert_eq!(host.state(&id), Some(PluginState::Crashed));
        // The host is still usable: register a different plugin.
        let other = VALID_MANIFEST.replacen("com.example.mermaid", "com.example.other", 1);
        let id2 = host.register_manifest(&other).expect("register again");
        assert_eq!(host.state(&id2), Some(PluginState::Active));
        // Crashed plugin is excluded from active().
        assert_eq!(host.active().filter(|p| &p.manifest.id == &id).count(), 0);
    }

    #[test]
    fn malformed_manifest_rejected() {
        let mut host = PluginHost::new();
        let err = host.register_manifest("not toml {{{").unwrap_err();
        assert!(matches!(err, PluginError::ManifestInvalid(_)));
    }

    #[test]
    fn missing_id_rejected() {
        let mut host = PluginHost::new();
        let bad = r#"
name = "NoId"
version = "1.0.0"
api_version = "1"
"#;
        assert!(host.register_manifest(bad).is_err());
    }

    #[test]
    fn deactivate_excludes_from_active() {
        let mut host = PluginHost::new();
        let id = host.register_manifest(VALID_MANIFEST).expect("register");
        host.deactivate(&id, PluginState::Deactivated);
        assert_eq!(host.active().filter(|p| &p.manifest.id == &id).count(), 0);
    }

    /// A crashed/deactivated plugin keeps its manifest but must lose all
    /// capabilities — otherwise a crashed plugin could still exercise
    /// permissions through `require_capability` (Invariant 8).
    #[test]
    fn crashed_plugin_loses_capabilities() {
        let mut host = PluginHost::new();
        let id = host.register_manifest(VALID_MANIFEST).expect("register");
        assert!(host.has_capability(&id, Capability::Network));
        host.handle_crash(&id, "trap");
        assert!(!host.has_capability(&id, Capability::Network));
        assert!(matches!(
            host.require_capability(&id, Capability::Network),
            Err(PluginError::PermissionDenied(_))
        ));
    }

    #[test]
    fn deactivated_plugin_loses_capabilities() {
        let mut host = PluginHost::new();
        let id = host.register_manifest(VALID_MANIFEST).expect("register");
        host.deactivate(&id, PluginState::Deactivated);
        assert!(!host.has_capability(&id, Capability::Network));
    }

    #[test]
    fn duplicate_active_id_rejected() {
        let mut host = PluginHost::new();
        host.register_manifest(VALID_MANIFEST).expect("register");
        let err = host.register_manifest(VALID_MANIFEST).unwrap_err();
        assert!(matches!(err, PluginError::DuplicateId(_)));
    }

    /// A crashed plugin must be re-registrable: after a crash the user fixes
    /// or updates the plugin and registers it again — blocking the id forever
    /// would force an editor restart to recover.
    #[test]
    fn crashed_plugin_can_be_reregistered() {
        let mut host = PluginHost::new();
        let id = host.register_manifest(VALID_MANIFEST).expect("register");
        host.handle_crash(&id, "trap");
        assert_eq!(host.state(&id), Some(PluginState::Crashed));

        // Re-register a FIXED manifest (e.g. with different permissions).
        let fixed = VALID_MANIFEST
            .replacen("network = true", "network = false", 1)
            .replacen("clipboard = false", "clipboard = true", 1);
        let id2 = host.register_manifest(&fixed).expect("re-register");
        assert_eq!(id, id2);
        assert_eq!(host.state(&id), Some(PluginState::Active));
        // New manifest wins: clipboard granted, network not.
        assert!(host.has_capability(&id, Capability::Clipboard));
        assert!(!host.has_capability(&id, Capability::Network));
        // Exactly one entry for the id — the crashed record was replaced.
        assert_eq!(
            host.plugins()
                .iter()
                .filter(|p| p.manifest.id == id)
                .count(),
            1
        );
    }

    #[test]
    fn deactivated_plugin_can_be_reregistered() {
        let mut host = PluginHost::new();
        let id = host.register_manifest(VALID_MANIFEST).expect("register");
        host.deactivate(&id, PluginState::Deactivated);
        host.register_manifest(VALID_MANIFEST).expect("re-register");
        assert_eq!(host.state(&id), Some(PluginState::Active));
    }
}
