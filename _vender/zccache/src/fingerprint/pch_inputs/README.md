# pch_inputs

Strict clang PCH input validation with content-verified mtime replay (#1952).

- `bitstream.rs` reads a clang PCH's `CONTROL_BLOCK` / `INPUT_FILES_BLOCK`
  records (spelling, size, mtime, xxh3 content hash): exactly what clang
  compares when a consumer loads the PCH. Only the head of the file is read.
- `mod.rs` decides whether a cached PCH may be served into the current
  workspace (`plan_pch_serve`) and replays the recorded mtime onto inputs whose
  content verified (`apply_pch_replay`).
- `tests.rs` builds real PCHs with `clang++` under strict flags
  (`-Werror=invalid-pch -verify-pch -fpch-validate-input-files-content`) and
  covers each decision branch.

Decision per recorded input: missing, size or content mismatch, or a PCH whose
top-level header is a different file (another checkout) is refused; an
mtime-only mismatch is repaired by setting the recorded mtime, but only for a
file under the replay root and only while `ZCCACHE_PCH_MTIME_REPLAY` is not
`0`/`false`/`off`/`no`.
