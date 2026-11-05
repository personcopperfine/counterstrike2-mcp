//! The explicit and implicit manifests must describe the same layer, at the same API version.
use serde_json::Value;

fn load(text: &str) -> Value {
    serde_json::from_str(text).expect("manifest is JSON")
}

#[test]
fn explicit_and_implicit_manifests_agree() {
    let explicit = load(include_str!("../VkLayer_cs2mcp_capture.json"));
    let implicit = load(include_str!("../implicit/VkLayer_cs2mcp_capture.json"));
    for key in ["name", "type", "api_version", "implementation_version"] {
        assert_eq!(
            explicit["layer"][key], implicit["layer"][key],
            "{key} differs"
        );
    }
    assert_eq!(explicit["layer"]["api_version"], "1.4.0");
    assert_eq!(explicit["layer"]["name"], "VK_LAYER_CS2MCP_capture");
    // relative library paths: resolved against each manifest's own directory
    assert!(explicit["layer"]["library_path"]
        .as_str()
        .unwrap()
        .starts_with("./"));
    assert!(implicit["layer"]["library_path"]
        .as_str()
        .unwrap()
        .starts_with("../"));
    assert_eq!(
        implicit["layer"]["enable_environment"]["CS2MCP_CAPTURE"],
        "1"
    );
    assert_eq!(
        implicit["layer"]["disable_environment"]["CS2MCP_CAPTURE_DISABLE"],
        "1"
    );
    assert!(explicit["layer"].get("enable_environment").is_none());
}

#[test]
fn the_windows_manifest_names_the_dll_next_to_it() {
    let explicit = load(include_str!("../VkLayer_cs2mcp_capture.json"));
    let windows = load(include_str!("../windows/VkLayer_cs2mcp_capture.json"));
    for key in ["name", "type", "api_version", "implementation_version"] {
        assert_eq!(explicit["layer"][key], windows["layer"][key], "{key} differs");
    }
    // a backslash: the Windows loader resolves a library_path against the manifest's directory
    // only when it contains its directory separator
    assert_eq!(windows["layer"]["library_path"], ".\\cs2_vk_capture.dll");
    assert!(windows["layer"].get("enable_environment").is_none());
}
