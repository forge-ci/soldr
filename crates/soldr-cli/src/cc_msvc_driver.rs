//! Driver-mode selection and the explicit hosted Windows runtime contract.
use std::path::Path;

use crate::core::SoldrError;

pub(super) fn config_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub(super) fn render_msvc_driver(
    clang: &Path,
    gnu_args: &[String],
    cl_args: &[String],
) -> Result<String, SoldrError> {
    let gnu = super::render_driver(clang, gnu_args)?;
    let cl = super::render_driver(clang, cl_args)?;
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return Ok(format!(
            "@echo off\r\nif /I \"%~n0\"==\"clang-cl\" goto clang_cl\r\nif /I \"%~n0\"==\"x86_64-pc-windows-msvc-clang-cl\" goto clang_cl\r\nif /I \"%~n0\"==\"aarch64-pc-windows-msvc-clang-cl\" goto clang_cl\r\nfor %%a in (%*) do if \"%%~a\"==\"--driver-mode=cl\" goto clang_cl\r\n{gnu}exit /b %errorlevel%\r\n:clang_cl\r\n{cl}"
        ));
    }
    Ok(format!(
        "#!/bin/sh\nmode=gcc\ncase \"$0\" in *clang-cl) mode=cl;; esac\nfor arg do\n  case \"$arg\" in\n    --) break;;\n    --driver-mode=cl) mode=cl;;\n    --driver-mode=gcc|--driver-mode=g++) mode=gcc;;\n  esac\ndone\nif [ \"$mode\" = cl ]; then\n{cl}fi\n{gnu}"
    ))
}

pub(super) fn msvc_runtime_policy(target: &str) -> String {
    serde_json::json!({
        "schema_version": 1,
        "target": target,
        "default_crt": "dynamic",
        "os_baseline": ["Windows 10+", "Windows Server 2016+"],
        "ucrt": "operating-system",
        "vc_runtime": "hosted-runner-System32",
        "redistributed_microsoft_dlls": [],
        "consumer_requirements": [
            "audit normal and delay imports recursively",
            "verify required VC runtime DLL architecture and bytes in System32 on the execution host",
            "stage every other non-OS dependency with provenance beside the test executables",
            "fail if any import is outside the staged closure or declared host baseline",
            "execute the original C reference and selected CTests on native Windows"
        ]
    }).to_string() + "\n"
}
