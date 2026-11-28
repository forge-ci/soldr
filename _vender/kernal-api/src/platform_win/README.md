# `platform_win`

The private Windows half of the host platform selection: concrete
implementations chosen by the one `std::cfg_select!` root in `src/lib.rs`
and reached only through root-level `use platform_imp::...;` bridges.
Neutral code must never name this tree — the `kernal_api_platform_boundary`
Dylint enforces that (docs/platform-boundary.md).

- `fs_materialize.rs` / `fs_*.rs` — cache-materialization mechanics:
  replacement, link classification, readonly attribute, path identity, change
  markers, volume facts, and the in-place write seal (`fs/write_seal.rs`).
- `fs/` — file-system submodules that need their own structure
  (owner-only directory DACLs, the write seal).
- `ipc*.rs` — named-pipe transport and per-user directory placement.
- `process_*.rs`, `terminal/`, `webview_*.rs` — process, console, and window
  mechanics behind the neutral facades.
