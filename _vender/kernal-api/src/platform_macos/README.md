# `platform_macos`

The private macOS half of the host platform selection: concrete
implementations chosen by the one `std::cfg_select!` root in `src/lib.rs`
and reached only through root-level `use platform_imp::...;` bridges.
Neutral code must never name this tree — the `kernal_api_platform_boundary`
Dylint enforces that (docs/platform-boundary.md).

Mirrors `platform_win` and `platform_linux` file-for-file where the neutral
facade demands a capability: `fs_materialize.rs` (replacement, link
classification, permission bits including the in-place write seal, change
markers, volume facts) plus the tree's own extent, watcher, IPC, process,
and window mechanics.
