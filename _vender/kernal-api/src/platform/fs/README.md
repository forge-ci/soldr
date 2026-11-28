# `platform::fs`

The neutral file-system facade: path-based operations whose contract lives
here and whose native halves the `fs_materialize` bridge selects per host.

- `materialize.rs` — links, permission bits (including the in-place write
  seal), change markers, volume facts, and the native path spelling used
  when materializing cached artifacts.
- `async_io.rs` — handle-based async file I/O.
- `path_file.rs` — path-observed file identity.
- `replacement.rs` — atomic replacement and directory installation.
- `temporary.rs` — temporary directory naming.
