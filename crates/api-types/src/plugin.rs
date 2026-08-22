use serde::{Deserialize, Serialize};

/// Describes a WASM plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginManifest {
    pub id: String,
    pub version: String,
    pub name: String,
    pub description: String,
    /// Host ABI version this plugin targets.
    #[serde(default = "default_abi_version")]
    pub abi_version: u32,
    pub capabilities_required: Vec<CapabilityRequest>,
    pub provides: Vec<PluginProvides>,
}

fn default_abi_version() -> u32 {
    1
}

/// A capability that a plugin requests from the host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[non_exhaustive]
pub enum CapabilityRequest {
    FilesystemRead { globs: Vec<String> },
    FilesystemWrite { globs: Vec<String> },
    NetworkOutbound { domains: Vec<String> },
    ShellExecute { allowlist: Vec<String> },
    Other { description: String },
}

/// What a plugin provides to the host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[non_exhaustive]
pub enum PluginProvides {
    Tool(ToolDescriptor),
    Provider(ProviderDescriptor),
    MemoryAdapter(AdapterDescriptor),
    /// Wire-serialization capability (ADR-53): a dialect plugin owns the
    /// request-body wire format for the provider it backs. It never touches
    /// transport or the filesystem — it is a pure string → string
    /// transformer behind [`PluginProvides::Provider`].
    Dialect(DialectDescriptor),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    /// JSON Schema describing the tool's input parameters.
    #[serde(default)]
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderDescriptor {
    pub name: String,
    pub model: String,
    /// Optional completion keepalive (ADR-53 §4): while the host awaits a
    /// slow plugin completion it emits a non-terminal liveness chunk every
    /// `heartbeat_interval_secs`. `None` disables the heartbeat.
    ///
    /// `#[serde(default)]` keeps existing provider manifests loadable.
    #[serde(default)]
    pub heartbeat_interval_secs: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdapterDescriptor {
    pub name: String,
    pub kind: String,
}

/// A dialect-plugin descriptor (ADR-53).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DialectDescriptor {
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test PluginManifest serialization round-trip.
    #[test]
    fn plugin_manifest_round_trip() {
        let manifest = PluginManifest {
            id: "test-plugin".to_string(),
            version: "1.0.0".to_string(),
            name: "Test Plugin".to_string(),
            description: "A test plugin".to_string(),
            abi_version: 1,
            capabilities_required: vec![],
            provides: vec![],
        };
        let json = serde_json::to_string(&manifest).expect("serialization should succeed");
        let deserialized: PluginManifest =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized.id, "test-plugin");
        assert_eq!(deserialized.version, "1.0.0");
        assert_eq!(deserialized.abi_version, 1);
    }

    /// Test PluginManifest default_abi_version() returns 1.
    #[test]
    fn plugin_manifest_default_abi_version() {
        assert_eq!(default_abi_version(), 1);
    }

    /// Test CapabilityRequest::FilesystemRead serialization and PartialEq.
    #[test]
    fn capability_filesystem_read() {
        let cap = CapabilityRequest::FilesystemRead {
            globs: vec!["*.rs".to_string(), "src/**".to_string()],
        };
        let json = serde_json::to_string(&cap).expect("serialization should succeed");
        assert!(json.contains("FilesystemRead"));
        let deserialized: CapabilityRequest =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized, cap);
    }

    /// Test CapabilityRequest::NetworkOutbound serialization.
    #[test]
    fn capability_network_outbound() {
        let cap = CapabilityRequest::NetworkOutbound {
            domains: vec!["api.example.com".to_string(), "*.google.com".to_string()],
        };
        let json = serde_json::to_string(&cap).expect("serialization should succeed");
        assert!(json.contains("NetworkOutbound"));
        let deserialized: CapabilityRequest =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized, cap);
    }

    /// Test CapabilityRequest::ShellExecute serialization.
    #[test]
    fn capability_shell_execute() {
        let cap = CapabilityRequest::ShellExecute {
            allowlist: vec!["git *".to_string(), "cargo build".to_string()],
        };
        let json = serde_json::to_string(&cap).expect("serialization should succeed");
        assert!(json.contains("ShellExecute"));
        let deserialized: CapabilityRequest =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized, cap);
    }

    /// Test CapabilityRequest::Other serialization with custom string.
    #[test]
    fn capability_other() {
        let cap = CapabilityRequest::Other { description: "Custom capability".to_string() };
        let json = serde_json::to_string(&cap).expect("serialization should succeed");
        assert!(json.contains("Other"));
        let deserialized: CapabilityRequest =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized, cap);
    }

    /// Test PluginProvides::Tool serialization with ToolDescriptor.
    #[test]
    fn plugin_provides_tool() {
        let provides = PluginProvides::Tool(ToolDescriptor {
            name: "my-tool".to_string(),
            description: "A custom tool".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        });
        let json = serde_json::to_string(&provides).expect("serialization should succeed");
        assert!(json.contains("Tool"));
        let deserialized: PluginProvides =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized, provides);
    }

    /// Test PluginProvides::Provider serialization with ProviderDescriptor.
    #[test]
    fn plugin_provides_provider() {
        let provides = PluginProvides::Provider(ProviderDescriptor {
            name: "custom-provider".to_string(),
            model: "custom-model".to_string(),
            heartbeat_interval_secs: None,
        });
        let json = serde_json::to_string(&provides).expect("serialization should succeed");
        assert!(json.contains("Provider"));
        let deserialized: PluginProvides =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized, provides);
    }

    /// Test PluginProvides::MemoryAdapter serialization with AdapterDescriptor.
    #[test]
    fn plugin_provides_memory_adapter() {
        let provides = PluginProvides::MemoryAdapter(AdapterDescriptor {
            name: "vector-store".to_string(),
            kind: "vector".to_string(),
        });
        let json = serde_json::to_string(&provides).expect("serialization should succeed");
        assert!(json.contains("MemoryAdapter"));
        let deserialized: PluginProvides =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized, provides);
    }

    /// Test PluginProvides::Dialect serialization with DialectDescriptor.
    #[test]
    fn plugin_provides_dialect() {
        let provides =
            PluginProvides::Dialect(DialectDescriptor { name: "custom-wire".to_string() });
        let json = serde_json::to_string(&provides).expect("serialization should succeed");
        assert!(json.contains("Dialect"), "json should tag the variant: {json}");
        assert!(json.contains("custom-wire"), "json should carry the name: {json}");
        let deserialized: PluginProvides =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized, provides);
    }

    /// A Provider manifest without `heartbeat_interval_secs` must deserialize
    /// with the heartbeat disabled (`None`) — additive, backward compatible.
    #[test]
    fn provider_descriptor_heartbeat_defaults_none() {
        let json = r#"{"name":"legacy-provider","model":"legacy-model"}"#;
        let descriptor: ProviderDescriptor =
            serde_json::from_str(json).expect("deserialization should succeed");
        assert_eq!(descriptor.name, "legacy-provider");
        assert_eq!(descriptor.model, "legacy-model");
        assert_eq!(descriptor.heartbeat_interval_secs, None);
    }

    /// A Provider descriptor with `heartbeat_interval_secs` round-trips.
    #[test]
    fn provider_descriptor_heartbeat_round_trip() {
        let descriptor = ProviderDescriptor {
            name: "hb-provider".to_string(),
            model: "hb-model".to_string(),
            heartbeat_interval_secs: Some(1),
        };
        let json = serde_json::to_string(&descriptor).expect("serialization should succeed");
        let deserialized: ProviderDescriptor =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized, descriptor);
    }

    /// Test ToolDescriptor default input_schema is null when not provided.
    #[test]
    fn tool_descriptor_default_input_schema() {
        let json = r#"{"name":"test","description":"test tool"}"#;
        let descriptor: ToolDescriptor =
            serde_json::from_str(json).expect("deserialization should succeed");
        assert_eq!(descriptor.name, "test");
        assert_eq!(descriptor.input_schema, serde_json::Value::Null);
    }
}
