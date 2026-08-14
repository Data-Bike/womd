//! TOML manifest parsing (§67).
//!
//! Manifest format (§67):
//! ```toml
//! id = "com.example.mermaid"
//! name = "Mermaid"
//! version = "1.0.0"
//! api_version = "1"
//!
//! [permissions]
//! filesystem = false
//! network = false
//! git_credentials = false
//! clipboard = false
//! process = false
//!
//! [contributes]
//! commands = true
//! markdown_extensions = true
//! renderers = true
//! ```

use editor_domain::ids::PluginId;
use editor_plugin_api::{ApiVersion, Capability, Contributes, PluginManifest};

/// Error parsing a plugin manifest.
#[derive(Debug)]
pub enum ManifestParseError {
    Toml(String),
    MissingField(&'static str),
    InvalidApiVersion(String),
}

impl core::fmt::Display for ManifestParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&match self {
            Self::Toml(s) => format!("toml parse error: {s}"),
            Self::MissingField(s) => format!("missing field: {s}"),
            Self::InvalidApiVersion(s) => format!("invalid api_version: {s}"),
        })
    }
}
impl std::error::Error for ManifestParseError {}

/// Parse a TOML manifest string into a `PluginManifest`.
pub fn parse_manifest(toml_text: &str) -> Result<PluginManifest, ManifestParseError> {
    let value: toml::Value = toml::from_str(toml_text).map_err(|e| ManifestParseError::Toml(e.to_string()))?;

    let id = value
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or(ManifestParseError::MissingField("id"))?
        .to_string();
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or(ManifestParseError::MissingField("name"))?
        .to_string();
    let version = value
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or(ManifestParseError::MissingField("version"))?
        .to_string();
    let api_version_str = value
        .get("api_version")
        .and_then(|v| v.as_str())
        .ok_or(ManifestParseError::MissingField("api_version"))?;
    let api_version = parse_api_version(api_version_str)?;

    let permissions = parse_permissions(value.get("permissions"))?;
    let contributes = parse_contributes(value.get("contributes"));

    Ok(PluginManifest {
        id: PluginId(id),
        name,
        version,
        api_version,
        permissions,
        contributes,
    })
}

fn parse_api_version(s: &str) -> Result<ApiVersion, ManifestParseError> {
    let parts: Vec<&str> = s.split('.').collect();
    let major = parts
        .first()
        .and_then(|p| p.parse::<u32>().ok())
        .ok_or_else(|| ManifestParseError::InvalidApiVersion(s.to_string()))?;
    let minor = parts.get(1).and_then(|p| p.parse::<u32>().ok()).unwrap_or(0);
    Ok(ApiVersion { major, minor })
}

fn parse_permissions(table: Option<&toml::Value>) -> Result<Vec<Capability>, ManifestParseError> {
    let Some(t) = table.and_then(|v| v.as_table()) else {
        return Ok(Vec::new());
    };
    let mut caps = Vec::new();
    let try_add = |caps: &mut Vec<Capability>, key: &str, cap: Capability| {
        if t.get(key).and_then(|v| v.as_bool()) == Some(true) {
            caps.push(cap);
        }
    };
    try_add(&mut caps, "filesystem", Capability::Filesystem);
    try_add(&mut caps, "network", Capability::Network);
    try_add(&mut caps, "git_credentials", Capability::GitCredentials);
    try_add(&mut caps, "clipboard", Capability::Clipboard);
    try_add(&mut caps, "process", Capability::Process);
    Ok(caps)
}

fn parse_contributes(table: Option<&toml::Value>) -> Contributes {
    let Some(t) = table.and_then(|v| v.as_table()) else {
        return Contributes::default();
    };
    let get = |key: &str| t.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
    Contributes {
        commands: get("commands"),
        markdown_extensions: get("markdown_extensions"),
        renderers: get("renderers"),
        themes: get("themes"),
        exporters: get("exporters"),
        importers: get("importers"),
        repository_providers: get("repository_providers"),
        asset_handlers: get("asset_handlers"),
        sidebar_panels: get("sidebar_panels"),
        status_bar_items: get("status_bar_items"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
id = "com.example.mermaid"
name = "Mermaid"
version = "1.0.0"
api_version = "1.2"

[permissions]
network = true

[contributes]
commands = true
markdown_extensions = true
"#;

    #[test]
    fn parses_valid_manifest() {
        let m = parse_manifest(VALID).expect("parse");
        assert_eq!(m.id.0, "com.example.mermaid");
        assert_eq!(m.name, "Mermaid");
        assert_eq!(m.version, "1.0.0");
        assert_eq!(m.api_version.major, 1);
        assert_eq!(m.api_version.minor, 2);
        assert!(m.permissions.contains(&Capability::Network));
        assert!(!m.permissions.contains(&Capability::Filesystem));
        assert!(m.contributes.commands);
        assert!(m.contributes.markdown_extensions);
        assert!(!m.contributes.themes);
    }

    #[test]
    fn missing_id_errors() {
        let bad = r#"
name = "x"
version = "1"
api_version = "1"
"#;
        assert!(matches!(parse_manifest(bad), Err(ManifestParseError::MissingField("id"))));
    }

    #[test]
    fn invalid_api_version_errors() {
        let bad = r#"
id = "x"
name = "x"
version = "1"
api_version = "abc"
"#;
        assert!(matches!(parse_manifest(bad), Err(ManifestParseError::InvalidApiVersion(_))));
    }

    #[test]
    fn empty_permissions_defaults_to_none() {
        let m = r#"
id = "x"
name = "x"
version = "1"
api_version = "1"
"#;
        let parsed = parse_manifest(m).expect("parse");
        assert!(parsed.permissions.is_empty());
    }
}
