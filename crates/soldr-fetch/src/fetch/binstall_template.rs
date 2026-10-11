//! Strict cargo-binstall `[package.metadata.binstall]` parsing and template
//! rendering (soldr#3726).
//!
//! Only the documented binstall variables render: `name`, `version`,
//! `target`, `archive-format` (alias `format`), `archive-suffix`, `repo`,
//! `binary-ext`, `target-family`, `target-arch`, `target-libc`,
//! `target-vendor`, and `bin` (in `bin-dir` only). Any other variable, an
//! unbalanced brace, or an empty `{}` is an error, never passed through.

use crate::core::{SoldrError, TargetTriple};

/// binstall `pkg-fmt` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PkgFmt {
    Tar,
    Tbz2,
    Tgz,
    Txz,
    Tzstd,
    Zip,
    Bin,
}

impl PkgFmt {
    pub(crate) fn parse(raw: &str) -> Result<Self, SoldrError> {
        Ok(match raw.trim() {
            "tar" => Self::Tar,
            "tbz2" => Self::Tbz2,
            "tgz" => Self::Tgz,
            "txz" => Self::Txz,
            "tzstd" => Self::Tzstd,
            "zip" => Self::Zip,
            "bin" => Self::Bin,
            other => {
                return Err(SoldrError::Other(format!(
                "binstall: unknown pkg-fmt `{other}` (known: tar, tbz2, tgz, txz, tzstd, zip, bin)"
            )))
            }
        })
    }

    /// `{ archive-format }`: binstall's first extension without the dot.
    fn archive_format(self) -> &'static str {
        match self {
            Self::Tar => "tar",
            Self::Tbz2 => "tbz2",
            Self::Tgz => "tgz",
            Self::Txz => "txz",
            Self::Tzstd => "tzstd",
            Self::Zip => "zip",
            Self::Bin => "bin",
        }
    }

    /// `{ archive-suffix }`.
    fn archive_suffix(self, target: &TargetTriple) -> &'static str {
        match self {
            Self::Tar => ".tar",
            Self::Tbz2 => ".tar.bz2",
            Self::Tgz => ".tar.gz",
            Self::Txz => ".tar.xz",
            Self::Tzstd => ".tar.zst",
            Self::Zip => ".zip",
            Self::Bin => target.binary_ext(),
        }
    }

    /// Suffix the shared extractor dispatches on (`""` = raw binary).
    /// `tbz2` has no decoder in soldr and is refused.
    pub(crate) fn extract_hint(self) -> Result<&'static str, SoldrError> {
        match self {
            Self::Tbz2 => Err(SoldrError::Other(
                "binstall: pkg-fmt `tbz2` is not supported by soldr".into(),
            )),
            Self::Bin => Ok(""),
            Self::Tar => Ok(".tar"),
            Self::Tgz => Ok(".tar.gz"),
            Self::Txz => Ok(".tar.xz"),
            Self::Tzstd => Ok(".tar.zst"),
            Self::Zip => Ok(".zip"),
        }
    }
}

/// Inputs to [`render`].
pub(crate) struct TemplateVars<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub target: &'a TargetTriple,
    pub pkg_fmt: PkgFmt,
    pub repo: Option<&'a str>,
    /// Set only when rendering `bin-dir`.
    pub bin: Option<&'a str>,
}

impl TemplateVars<'_> {
    fn lookup(&self, key: &str) -> Result<String, SoldrError> {
        let triple = self.target.triple();
        let parts: Vec<&str> = triple.split('-').collect();
        let value = match key {
            "name" => self.name.to_string(),
            "version" => self.version.to_string(),
            "target" => triple.clone(),
            "archive-format" | "format" => self.pkg_fmt.archive_format().to_string(),
            "archive-suffix" => self.pkg_fmt.archive_suffix(self.target).to_string(),
            "binary-ext" => self.target.binary_ext().to_string(),
            "target-arch" => parts.first().copied().unwrap_or_default().to_string(),
            "target-vendor" => parts.get(1).copied().unwrap_or_default().to_string(),
            "target-family" => if triple.contains("windows") {
                "windows"
            } else if triple.starts_with("wasm") {
                "wasm"
            } else {
                "unix"
            }
            .to_string(),
            "target-libc" => {
                let env = parts.get(3).copied().unwrap_or_default();
                if parts.len() >= 4 {
                    env.to_string()
                } else {
                    String::new()
                }
            }
            "repo" => self
                .repo
                .ok_or_else(|| {
                    SoldrError::Other(format!(
                        "binstall: template uses {{ repo }} but {} has no package.repository",
                        self.name
                    ))
                })?
                .trim_end_matches('/')
                .to_string(),
            "bin" if self.bin.is_some() => self.bin.unwrap_or_default().to_string(),
            other => {
                return Err(SoldrError::Other(format!(
                    "binstall: unknown template variable `{other}`"
                )))
            }
        };
        Ok(value)
    }
}

/// Render a binstall template, refusing anything not strictly documented.
pub(crate) fn render(template: &str, vars: &TemplateVars<'_>) -> Result<String, SoldrError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find(['{', '}']) {
        if rest[open..].starts_with('}') {
            return Err(SoldrError::Other(format!(
                "binstall: unbalanced `}}` in template `{template}`"
            )));
        }
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after.find('}').ok_or_else(|| {
            SoldrError::Other(format!("binstall: unclosed `{{` in template `{template}`"))
        })?;
        let key = after[..close].trim();
        if key.is_empty() || key.contains('{') {
            return Err(SoldrError::Other(format!(
                "binstall: malformed variable in template `{template}`"
            )));
        }
        out.push_str(&vars.lookup(key)?);
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Resolved binstall metadata for one target (overrides applied).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BinstallMeta {
    pub pkg_url: String,
    pub pkg_fmt: PkgFmt,
    pub bin_dir: Option<String>,
    pub repo: Option<String>,
}

impl BinstallMeta {
    /// Parse a crate's `Cargo.toml`. `Ok(None)` when it declares no
    /// `pkg-url` for `target` (soldr does not guess binstall's defaults).
    pub(crate) fn from_cargo_toml(
        text: &str,
        target: &TargetTriple,
    ) -> Result<Option<Self>, SoldrError> {
        let doc: toml::Value = text
            .parse()
            .map_err(|e| SoldrError::Other(format!("binstall: invalid Cargo.toml: {e}")))?;
        let package = doc.get("package");
        let repo = package
            .and_then(|p| p.get("repository"))
            .and_then(toml::Value::as_str)
            .map(str::to_string);
        let Some(meta) = package
            .and_then(|p| p.get("metadata"))
            .and_then(|m| m.get("binstall"))
        else {
            return Ok(None);
        };
        let over = meta
            .get("overrides")
            .and_then(|o| o.get(target.triple().as_str()));
        let field = |key: &str| {
            over.and_then(|o| o.get(key))
                .or_else(|| meta.get(key))
                .and_then(toml::Value::as_str)
                .map(str::to_string)
        };
        let Some(pkg_url) = field("pkg-url") else {
            return Ok(None);
        };
        let pkg_fmt = match field("pkg-fmt") {
            Some(raw) => PkgFmt::parse(&raw)?,
            None => PkgFmt::Tgz,
        };
        Ok(Some(Self {
            pkg_url,
            pkg_fmt,
            bin_dir: field("bin-dir"),
            repo,
        }))
    }
}
