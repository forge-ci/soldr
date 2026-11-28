//! Public contract of `build_resources`, the build-script helper that embeds
//! a Windows executable's icon, version information and manifest. The resource
//! script rendering itself is covered by the module's unit tests.
#![cfg(feature = "build-resources")]

use kernal_api::build_resources::{
    embed_windows_app_resources, WindowsAppResources, WindowsResourceError,
};

#[test]
fn descriptions_accept_semver_and_reject_unrepresentable_versions() {
    assert!(WindowsAppResources::new("FastLED Viewer", "fastled", "2.0.20").is_ok());
    assert!(WindowsAppResources::new("FastLED Viewer", "fastled", "1.2.3-beta.4+build.5").is_ok());
    for version in ["2", "2.0", "2.0.x", "2.0.20.1", "70000.0.0", "2..0"] {
        let error = WindowsAppResources::new("App", "Company", version).unwrap_err();
        assert!(
            matches!(error, WindowsResourceError::InvalidVersion(ref got) if got == version),
            "{version:?}: {error}"
        );
    }
}

#[test]
fn text_fields_reject_values_a_resource_script_cannot_hold() {
    let long = "x".repeat(257);
    for bad in ["", "a\"b", "a\nb", long.as_str()] {
        assert!(matches!(
            WindowsAppResources::new(bad, "company", "1.0.0"),
            Err(WindowsResourceError::InvalidText {
                field: "product name"
            })
        ));
        assert!(matches!(
            WindowsAppResources::new("App", bad, "1.0.0"),
            Err(WindowsResourceError::InvalidText {
                field: "company name"
            })
        ));
        let described = WindowsAppResources::new("App", "Company", "1.0.0")
            .unwrap()
            .with_file_description(bad);
        assert!(matches!(
            described,
            Err(WindowsResourceError::InvalidText {
                field: "file description"
            })
        ));
    }
}

#[test]
fn embedding_outside_a_build_script_is_a_typed_error() {
    // Cargo sets `CARGO_CFG_TARGET_OS` for build scripts, never for tests.
    if std::env::var_os("CARGO_CFG_TARGET_OS").is_some() {
        return;
    }
    let resources = WindowsAppResources::new("App", "Company", "1.0.0")
        .unwrap()
        .with_icon("icons/icon.ico");
    let error = embed_windows_app_resources(&resources).unwrap_err();
    assert!(
        matches!(
            error,
            WindowsResourceError::NotInBuildScript("CARGO_CFG_TARGET_OS")
        ),
        "{error}"
    );
    assert!(error.to_string().contains("build script"), "{error}");
}
