use super::*;
use semver::VersionReq;

#[test]
fn test_pixi_version_validation() {
    let toml = r#"
        [workspace]
        name = "test_project"
        channels = []
        platforms = []
        pixi-version = ">=0.1.0"
    "#;

    let result = TableName::from_toml_str(toml);
    assert!(result.is_ok());
}

#[test]
fn test_pixi_version_validation_failure() {
    let toml = r#"
        [workspace]
        name = "test_project"
        channels = []
        platforms = []
        pixi-version = ">=999.0.0"
    "#;

    let result = TableName::from_toml_str(toml);
    assert!(result.is_err());
    if let Err(TomlError::PixiVersionError(msg)) = result {
        assert!(msg.contains("Pixi version"));
    } else {
        panic!("Expected PixiVersionError");
    }
}

#[test]
fn test_pixi_version_validation_invalid_spec() {
    let toml = r#"
        [workspace]
        name = "test_project"
        channels = []
        platforms = []
        pixi-version = "invalid_spec"
    "#;

    let result = TableName::from_toml_str(toml);
    assert!(result.is_err());
    if let Err(TomlError::PixiVersionError(msg)) = result {
        assert!(msg.contains("Invalid Pixi version requirement"));
    } else {
        panic!("Expected PixiVersionError");
    }
}
