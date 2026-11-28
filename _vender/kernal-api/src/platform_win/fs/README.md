# `platform_win::fs`

Windows-only file-system submodules that need more structure than a flat
platform file carries.

- `private_directory.rs` — owner-only directory DACLs (`ensure_dir_private`,
  `create_dir_all_private`): which trustees a deploy-directory DACL names.
- `write_seal.rs` — in-place write sealing without the `READONLY` attribute
  (`deny_in_place_writes` / `allow_in_place_writes` /
  `in_place_writes_denied`): a deny ACE for write-data keeps the owner from
  writing a file in place while `Permissions::readonly()` stays false and
  rename-over-replace still works (zccache#1791).
