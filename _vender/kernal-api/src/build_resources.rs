//! Windows application resources for a binary's own build script.
//!
//! An executable's Explorer icon, version information and application manifest
//! are resources linked into that executable. Cargo links a build script's
//! resources only into the binaries of the package that runs it, so an
//! application calls [`embed_windows_app_resources`] from its own `build.rs`.
//! On every other target the call does nothing.
//!
//! # Depending on it from a build script
//!
//! Add `kernal-api` a second time, as a build-dependency with only this
//! feature, and enable the application's runtime capabilities in its
//! `[dependencies]` entry:
//!
//! ```toml
//! [dependencies]
//! kernal-api = { version = "=0.1.17", features = ["window-icon"] }
//!
//! [build-dependencies]
//! kernal-api = { version = "=0.1.17", default-features = false, features = ["build-resources"] }
//! ```
//!
//! Cargo's edition-2021 feature resolver resolves build-dependency features
//! separately from the runtime graph's, so `window-icon` above never reaches
//! the build script and `build-resources` never reaches the executable.
//!
//! One Cargo rule crosses that line: a `kernal-api/<feature>` entry in the
//! application's own `[features]` table applies to *every* dependency named
//! `kernal-api`, the build-dependency included. An application feature such
//! as `viewer = ["kernal-api/tauri-webview"]` therefore also compiles the
//! webview stack for the host build script. The build-dependency cannot be
//! renamed out of reach: Cargo rejects one package depending on another under
//! two names ("depends on crate ... multiple times with different names"),
//! whatever the dependency kinds. An application that forwards a heavy
//! capability either accepts that host compile, or moves the forwarding into
//! an application-owned library crate (`viewer = ["dep:app-viewer"]`, with
//! `app-viewer` enabling the kernal-api capability), so its binary package's
//! feature table never names `kernal-api/...`. `tests/build-resources-consumer`
//! and `ci/check_compilation_boundary_dependencies.py` pin both behaviours.
//!
//! Even with no forwarded features, the build script compiles `kernal-api`'s
//! mandatory dependencies -- the process substrate, Tokio, BLAKE3 and the
//! host platform crates -- for the host. Only the optional capabilities stay
//! out.
//!
//! ```no_run
//! use kernal_api::build_resources::{embed_windows_app_resources, WindowsAppResources};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let resources = WindowsAppResources::new("Example Viewer", "example", env!("CARGO_PKG_VERSION"))?
//!     .with_icon("icons/icon.ico");
//! embed_windows_app_resources(&resources)?;
//! # Ok(())
//! # }
//! ```
//!
//! The resource compiler is resolved privately from the build script's
//! environment, not from the host this crate was compiled for: `rc.exe` from
//! the Windows SDK on MSVC hosts, `llvm-rc` when cross-compiling to
//! `*-pc-windows-msvc`, and the `RC`/`RC_<target>` environment variables
//! override both. The target is read from Cargo's `CARGO_CFG_TARGET_OS`.

use std::path::{Path, PathBuf};

/// Resource id of the application icon group. `IDI_APPLICATION`'s value, which
/// is also what Tauri-built executables used, so window classes and shells that
/// ask for the default application icon find this one.
const ICON_GROUP_ID: u16 = 32512;

/// Longest accepted product, company or description string, in UTF-8 bytes.
const MAX_TEXT_BYTES: usize = 256;

/// Activates Common Controls v6, which themed native dialogs and controls need.
const COMMON_CONTROLS_MANIFEST: &str = r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity
        type="win32"
        name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0"
        processorArchitecture="*"
        publicKeyToken="6595b64144ccf1df"
        language="*"
      />
    </dependentAssembly>
  </dependency>
</assembly>
"#;

/// The resources embedded into a Windows executable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowsAppResources {
    product_name: String,
    company_name: String,
    file_description: String,
    version: [u16; 4],
    version_text: String,
    icon: Option<PathBuf>,
}

impl WindowsAppResources {
    /// Describes an application. `version` is the package's SemVer version,
    /// normally `env!("CARGO_PKG_VERSION")`; its major, minor and patch become
    /// the numeric file and product versions, and any pre-release or build
    /// suffix stays in the version strings. The file description defaults to
    /// the product name.
    pub fn new(
        product_name: &str,
        company_name: &str,
        version: &str,
    ) -> Result<Self, WindowsResourceError> {
        validate_text("product name", product_name)?;
        validate_text("company name", company_name)?;
        validate_text("version", version)?;
        Ok(Self {
            product_name: product_name.to_owned(),
            company_name: company_name.to_owned(),
            file_description: product_name.to_owned(),
            version: numeric_version(version)?,
            version_text: version.to_owned(),
            icon: None,
        })
    }

    /// Sets the text Explorer shows as the file description.
    pub fn with_file_description(
        mut self,
        description: &str,
    ) -> Result<Self, WindowsResourceError> {
        validate_text("file description", description)?;
        self.file_description = description.to_owned();
        Ok(self)
    }

    /// Embeds an `.ico` file as the application icon. A relative path is
    /// resolved against the calling package's manifest directory.
    pub fn with_icon(mut self, icon: impl Into<PathBuf>) -> Self {
        self.icon = Some(icon.into());
        self
    }
}

/// Why resources could not be embedded.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WindowsResourceError {
    /// A text field is empty, too long, or contains a control character or `"`.
    #[error("{field} must be 1-{MAX_TEXT_BYTES} bytes without control characters or quotes")]
    InvalidText { field: &'static str },
    /// The version does not start with numeric `major.minor.patch` parts that
    /// each fit in 16 bits.
    #[error("version {0:?} must start with numeric major.minor.patch parts of at most 65535")]
    InvalidVersion(String),
    /// A variable Cargo sets for build scripts is missing: this was not called
    /// from a build script.
    #[error("{0} is not set; Windows resources can only be embedded from a build script")]
    NotInBuildScript(&'static str),
    /// The icon file does not exist.
    #[error("icon file {} does not exist", .0.display())]
    MissingIcon(PathBuf),
    /// Writing the generated resource script failed.
    #[error("cannot write the resource script: {0}")]
    Io(#[source] std::io::Error),
    /// The resource compiler was unavailable or rejected the script.
    #[error("cannot compile Windows resources: {0}")]
    Compile(String),
}

/// Compiles `resources` and links them into the calling package's binaries.
///
/// Call this from `build.rs`. It does nothing unless the target OS is Windows.
/// It prints the `cargo:rerun-if-changed` line for the icon itself.
pub fn embed_windows_app_resources(
    resources: &WindowsAppResources,
) -> Result<(), WindowsResourceError> {
    let target_os = build_env("CARGO_CFG_TARGET_OS")?;
    if target_os != "windows" {
        return Ok(());
    }
    let out_dir = PathBuf::from(build_env("OUT_DIR")?);
    let manifest_dir = PathBuf::from(build_env("CARGO_MANIFEST_DIR")?);

    let icon = match &resources.icon {
        Some(icon) => {
            let icon = if icon.is_absolute() {
                icon.clone()
            } else {
                manifest_dir.join(icon)
            };
            if !icon.is_file() {
                return Err(WindowsResourceError::MissingIcon(icon));
            }
            println!("cargo:rerun-if-changed={}", icon.display());
            Some(icon)
        }
        None => None,
    };

    let manifest = out_dir.join("kernal-api-app.manifest");
    std::fs::write(&manifest, COMMON_CONTROLS_MANIFEST).map_err(WindowsResourceError::Io)?;
    let script = out_dir.join("kernal-api-app.rc");
    std::fs::write(
        &script,
        render_resource_script(resources, icon.as_deref(), &manifest)?,
    )
    .map_err(WindowsResourceError::Io)?;

    embed_resource::compile(&script, embed_resource::NONE)
        .manifest_required()
        .map_err(|result| WindowsResourceError::Compile(result.to_string()))
}

fn build_env(variable: &'static str) -> Result<String, WindowsResourceError> {
    std::env::var(variable).map_err(|_| WindowsResourceError::NotInBuildScript(variable))
}

fn validate_text(field: &'static str, text: &str) -> Result<(), WindowsResourceError> {
    if text.is_empty()
        || text.len() > MAX_TEXT_BYTES
        || text.chars().any(|c| c.is_control() || c == '"')
    {
        return Err(WindowsResourceError::InvalidText { field });
    }
    Ok(())
}

fn numeric_version(version: &str) -> Result<[u16; 4], WindowsResourceError> {
    let invalid = || WindowsResourceError::InvalidVersion(version.to_owned());
    let core = version.split(['-', '+']).next().unwrap_or_default();
    let mut parts = core.split('.');
    let mut numbers = [0u16; 4];
    for number in numbers.iter_mut().take(3) {
        let part = parts.next().ok_or_else(invalid)?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        *number = part.parse().map_err(|_| invalid())?;
    }
    if parts.next().is_some() {
        return Err(invalid());
    }
    Ok(numbers)
}

/// A resource-script string literal: backslashes are escaped; quotes and
/// control characters were already rejected.
fn rc_path(path: &Path) -> Result<String, WindowsResourceError> {
    let text = path.to_string_lossy();
    if text.chars().any(|c| c.is_control() || c == '"') {
        return Err(WindowsResourceError::InvalidText { field: "path" });
    }
    Ok(text.replace('\\', "\\\\"))
}

fn render_resource_script(
    resources: &WindowsAppResources,
    icon: Option<&Path>,
    manifest: &Path,
) -> Result<String, WindowsResourceError> {
    let [major, minor, patch, build] = resources.version;
    let mut script = String::from("#pragma code_page(65001)\n");
    script.push_str(&format!("1 24 \"{}\"\n", rc_path(manifest)?));
    if let Some(icon) = icon {
        script.push_str(&format!("{ICON_GROUP_ID} ICON \"{}\"\n", rc_path(icon)?));
    }
    script.push_str(&format!(
        "1 VERSIONINFO\n\
         FILEVERSION {major},{minor},{patch},{build}\n\
         PRODUCTVERSION {major},{minor},{patch},{build}\n\
         FILEOS 0x40004\n\
         FILETYPE 0x1\n\
         BEGIN\n\
         \x20 BLOCK \"StringFileInfo\"\n\
         \x20 BEGIN\n\
         \x20   BLOCK \"040904B0\"\n\
         \x20   BEGIN\n\
         \x20     VALUE \"CompanyName\", \"{company}\"\n\
         \x20     VALUE \"FileDescription\", \"{description}\"\n\
         \x20     VALUE \"FileVersion\", \"{version}\"\n\
         \x20     VALUE \"ProductName\", \"{product}\"\n\
         \x20     VALUE \"ProductVersion\", \"{version}\"\n\
         \x20   END\n\
         \x20 END\n\
         \x20 BLOCK \"VarFileInfo\"\n\
         \x20 BEGIN\n\
         \x20   VALUE \"Translation\", 0x0409, 0x04B0\n\
         \x20 END\n\
         END\n",
        company = resources.company_name,
        description = resources.file_description,
        version = resources.version_text,
        product = resources.product_name,
    ));
    Ok(script)
}

// Rendering is private; the public validation contract is tested in
// tests/process/windows_app_resources.rs.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_embeds_manifest_icon_and_version_strings() {
        let resources = WindowsAppResources::new("FastLED Viewer", "fastled", "2.0.20-rc.1")
            .unwrap()
            .with_file_description("FastLED CLI")
            .unwrap()
            .with_icon("icons/icon.ico");
        let script = render_resource_script(
            &resources,
            Some(Path::new(r"C:\app\icons\icon.ico")),
            Path::new(r"C:\out\kernal-api-app.manifest"),
        )
        .unwrap();
        assert!(
            script.contains("1 24 \"C:\\\\out\\\\kernal-api-app.manifest\"\n"),
            "{script}"
        );
        assert!(
            script.contains("32512 ICON \"C:\\\\app\\\\icons\\\\icon.ico\"\n"),
            "{script}"
        );
        assert!(script.contains("FILEVERSION 2,0,20,0\n"), "{script}");
        assert!(
            script.contains("VALUE \"CompanyName\", \"fastled\""),
            "{script}"
        );
        assert!(
            script.contains("VALUE \"FileDescription\", \"FastLED CLI\""),
            "{script}"
        );
        assert!(
            script.contains("VALUE \"ProductName\", \"FastLED Viewer\""),
            "{script}"
        );
        assert!(
            script.contains("VALUE \"ProductVersion\", \"2.0.20-rc.1\""),
            "{script}"
        );
        assert!(
            script.contains("VALUE \"Translation\", 0x0409, 0x04B0"),
            "{script}"
        );
    }

    #[test]
    fn script_without_icon_has_no_icon_resource() {
        let resources = WindowsAppResources::new("App", "Company", "0.1.0").unwrap();
        let script =
            render_resource_script(&resources, None, Path::new("/out/app.manifest")).unwrap();
        assert!(!script.contains(" ICON "), "{script}");
        assert!(script.contains("1 24 \"/out/app.manifest\"\n"), "{script}");
    }

    #[test]
    fn manifest_activates_common_controls_v6() {
        assert!(COMMON_CONTROLS_MANIFEST.contains("Microsoft.Windows.Common-Controls"));
        assert!(COMMON_CONTROLS_MANIFEST.contains("version=\"6.0.0.0\""));
    }
}
