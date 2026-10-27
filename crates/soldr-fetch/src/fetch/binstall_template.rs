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
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
pub(crate) enum PkgFmt {
    #[strum(serialize = "tar")]
    Tar,
    #[strum(serialize = "tbz2")]
    Tbz2,
    #[strum(serialize = "tgz")]
    Tgz,
    #[strum(serialize = "txz")]
    Txz,
    #[strum(serialize = "tzstd")]
    Tzstd,
    #[strum(serialize = "zip")]
    Zip,
    #[strum(serialize = "bin")]
    Bin,
}

impl PkgFmt {
    pub(crate) fn parse(raw: &str) -> Result<Self, SoldrError> {
        raw.trim().parse().map_err(|_| {
            SoldrError::Other(format!(
                "binstall: unknown pkg-fmt `{}` (known: tar, tbz2, tgz, txz, tzstd, zip, bin)",
                raw.trim()
            ))
        })
    }

    /// `{ archive-format }`: binstall's first extension without the dot.
    fn archive_format(self) -> &'static str {
        self.into()
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
    pub(crate) fn extract_hint(self) -> &'static str {
        match self {
            Self::Bin => "",
            Self::Tar => ".tar",
            Self::Tbz2 => ".tar.bz2",
            Self::Tgz => ".tar.gz",
            Self::Txz => ".tar.xz",
            Self::Tzstd => ".tar.zst",
            Self::Zip => ".zip",
        }
    }

    /// Formats binstall's default guesses try when no `pkg-fmt` is given,
    /// in binstall's order.
    const GUESS_ORDER: [Self; 7] = [
        Self::Tgz,
        Self::Tbz2,
        Self::Txz,
        Self::Tzstd,
        Self::Tar,
        Self::Zip,
        Self::Bin,
    ];
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
    /// Set only when rendering `signing.file` (`{ url }` = the asset URL).
    pub url: Option<&'a str>,
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
            "url" if self.url.is_some() => self.url.unwrap_or_default().to_string(),
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

/// binstall's default `pkg-url` guesses, tried when a crate declares none
/// (cargo-binstall `DEFAULT_PKG_URLS`, GitHub-style release layout).
const DEFAULT_PKG_URLS: [&str; 9] = [
    "{ repo }/releases/download/{ version }/{ name }-{ target }-v{ version }{ archive-suffix }",
    "{ repo }/releases/download/v{ version }/{ name }-{ target }-v{ version }{ archive-suffix }",
    "{ repo }/releases/download/{ version }/{ name }-{ target }-{ version }{ archive-suffix }",
    "{ repo }/releases/download/v{ version }/{ name }-{ target }-{ version }{ archive-suffix }",
    "{ repo }/releases/download/{ version }/{ name }-{ version }-{ target }{ archive-suffix }",
    "{ repo }/releases/download/v{ version }/{ name }-{ version }-{ target }{ archive-suffix }",
    "{ repo }/releases/download/v{ version }/{ name }-v{ version }-{ target }{ archive-suffix }",
    "{ repo }/releases/download/{ version }/{ name }-{ target }{ archive-suffix }",
    "{ repo }/releases/download/v{ version }/{ name }-{ target }{ archive-suffix }",
];

/// `[package.metadata.binstall.signing]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Signing {
    pub algorithm: String,
    pub pubkey: String,
    /// Signature URL template; binstall's default is `{ url }.sig`.
    pub file: Option<String>,
}

/// Resolved binstall metadata for one target (overrides applied).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BinstallMeta {
    /// `None` = no declared `pkg-url`; binstall's default guesses apply.
    pub pkg_url: Option<String>,
    /// `None` = undeclared (`tgz` for a declared `pkg-url`, every format
    /// for the default guesses).
    pub pkg_fmt: Option<PkgFmt>,
    pub bin_dir: Option<String>,
    pub repo: Option<String>,
    pub signing: Option<Signing>,
}

impl BinstallMeta {
    /// Parse a crate's `Cargo.toml`. A crate with no binstall table yields
    /// all-`None` metadata, i.e. the default guesses.
    pub(crate) fn from_cargo_toml(text: &str, target: &TargetTriple) -> Result<Self, SoldrError> {
        let doc: toml::Value = text
            .parse()
            .map_err(|e| SoldrError::Other(format!("binstall: invalid Cargo.toml: {e}")))?;
        let package = doc.get("package");
        let repo = package
            .and_then(|p| p.get("repository"))
            .and_then(toml::Value::as_str)
            .map(str::to_string);
        let meta = package
            .and_then(|p| p.get("metadata"))
            .and_then(|m| m.get("binstall"));
        let over = meta
            .and_then(|m| m.get("overrides"))
            .and_then(|o| o.get(target.triple().as_str()));
        let field = |key: &str| {
            over.and_then(|o| o.get(key))
                .or_else(|| meta.and_then(|m| m.get(key)))
                .and_then(toml::Value::as_str)
                .map(str::to_string)
        };
        let pkg_fmt = field("pkg-fmt")
            .map(|raw| PkgFmt::parse(&raw))
            .transpose()?;
        let signing = over
            .and_then(|o| o.get("signing"))
            .or_else(|| meta.and_then(|m| m.get("signing")))
            .map(|table| {
                let get = |key: &str| table.get(key).and_then(toml::Value::as_str);
                Signing {
                    algorithm: get("algorithm").unwrap_or_default().to_string(),
                    pubkey: get("pubkey").unwrap_or_default().to_string(),
                    file: get("file").map(str::to_string),
                }
            });
        Ok(Self {
            pkg_url: field("pkg-url"),
            pkg_fmt,
            bin_dir: field("bin-dir"),
            repo,
            signing,
        })
    }

    /// Candidate `(url, pkg-fmt)` pairs, in probe order. The declared
    /// `pkg-url` alone when present; otherwise binstall's default guesses
    /// (empty when the crate has no `package.repository`).
    pub(crate) fn candidates(
        &self,
        name: &str,
        version: &str,
        target: &TargetTriple,
    ) -> Result<Vec<(String, PkgFmt)>, SoldrError> {
        let vars = |pkg_fmt| TemplateVars {
            name,
            version,
            target,
            pkg_fmt,
            repo: self.repo.as_deref(),
            bin: None,
            url: None,
        };
        if let Some(pkg_url) = &self.pkg_url {
            let fmt = self.pkg_fmt.unwrap_or(PkgFmt::Tgz);
            return Ok(vec![(render(pkg_url, &vars(fmt))?, fmt)]);
        }
        if self.repo.is_none() {
            return Ok(Vec::new());
        }
        let fmts: Vec<PkgFmt> = match self.pkg_fmt {
            Some(fmt) => vec![fmt],
            None => PkgFmt::GUESS_ORDER.to_vec(),
        };
        let mut out = Vec::new();
        for template in DEFAULT_PKG_URLS {
            for &fmt in &fmts {
                out.push((render(template, &vars(fmt))?, fmt));
            }
        }
        Ok(out)
    }
}
