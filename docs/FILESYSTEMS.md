# Filesystems under `target/` and the soldr cache

This is the decision record for soldr#3093 (filesystem behaviour) and soldr#3092
(per-worktree target dirs). It covers what soldr already does for multi-worktree
Rust builds and what the filesystem underneath changes. It also says what to
tune and what soldr deliberately leaves to the user.

## What soldr already does

- **A cache hit is a link or a clone, not a copy.** By default, soldr probes the
  pair of directories a hit moves between, which are the zccache store and the
  Cargo target dir. It then injects one explicit `ZCCACHE_MODE`:
  - `REFLINK` where the volume can clone;
  - otherwise `LINK` (a hardlink);
  - otherwise `COPY`.

  The probe and injection are soldr#3440. `SOLDR_ZCCACHE_MODE` and
  `--zccache-mode` override them (see `docs/API.md`).
- **Sibling worktrees share hits.** `ZCCACHE_PATH_REMAP=auto` makes artifacts
  path-independent, so worktree B hits what worktree A compiled (soldr#352).
- **GC counts a hardlinked file once.** `soldr gc` sizes a directory by unique
  file (soldr#3439). A hardlink shared between the cache and several `target/`
  dirs is not counted once per name, so the reclaimable figure is not inflated
  by the number of worktrees. Reflink sharing and Windows hardlinks cannot be
  seen through the file API, so on those the figure is an upper bound. For what
  a deletion actually frees, trust `df`.
- **One shared `CARGO_TARGET_DIR` is rejected.** It funnels every worktree
  through one `.cargo-lock` and makes them unify features against each other.
  See `docs/TARGET_GC_PROPOSAL.md`.

## Filesystem table

| Filesystem | Clone (reflink) | Hardlink | Cost of rewriting an object | Default compression / checksums | Snapshot pinning | What soldr's probe picks | Recommendation |
|---|---|---|---|---|---|---|---|
| ext4 | no | yes | none | none | none, unless LVM snapshots | `LINK` | Nothing to tune; this is the baseline. |
| XFS (`reflink=1`, the mkfs default since xfsprogs 5.1) | yes | yes | only for shared extents | none | none | `REFLINK` | Best Linux default for a zccache-backed build. |
| btrfs, default mount | yes | yes | high: every write is a new extent (CoW) | checksums on; zstd if mounted with `compress` | high with snapper/timeshift | `REFLINK` | Use the tuned row below. |
| btrfs, dedicated subvolume + `chattr +C` | yes, but a reflinked file is CoW'd once on first write | yes | none for files created after `+C` | off for `+C` files | none: a nested subvolume is excluded from snapshots of its parent | `REFLINK` | Create the subvolume first, set `+C` on the empty directory, then build. `+C` is not retroactive. |
| ZFS | block cloning since OpenZFS 2.2, off by default (`zfs_bclone_enabled`) | yes | high (CoW) | lz4 and checksums on | high | `LINK` unless cloning is enabled | A dedicated dataset for build output, with snapshots off. |
| APFS (macOS) | yes (`clonefile`) | yes | low | none | Time Machine local snapshots can pin space | `REFLINK` | Exclude `target/` and `~/.soldr` from Time Machine. |
| NTFS | no | yes | none | none | VSS restore points rarely matter | `LINK` | Add Defender exclusions (`soldr doctor` reports these). |
| ReFS / Dev Drive (Windows) | yes (block cloning) | no | low | integrity streams off by default on Dev Drive | none | `REFLINK` | Prefer a Dev Drive for `target/` and the soldr cache. |

btrfs is not faster than ext4 for Rust builds by default. The tuned row only
makes it *not slower*. Clones are the one thing XFS, btrfs, APFS and ReFS
have over ext4/NTFS: each worktree gets an independent writable copy of a cache
hit for free.

## Decisions

1. **soldr documents filesystem tuning and does not change it.** Creating a
   subvolume, running `chattr +C`, changing mount options or dataset properties
   are host-administration actions. soldr never performs them.
2. **Per-worktree target isolation (soldr#3092): detect first, do not
   provision.** soldr does not write a worktree's `.cargo/config.toml` or
   choose its target dir; the repository or agent harness that creates the
   worktree owns that. The recommended layout is one target dir per worktree,
   with cross-worktree reuse coming from the zccache layer above. A
   detection probe in `soldr doctor` is still worth adding: warn when one target
   dir has been written by more than one workspace root. That probe is a
   separate, scoped change. The compile request already carries both inputs
   (`cwd`, `target_dir`), but nothing joins them yet.
3. **No filesystem cell in the perf matrix yet.** Hosted runners give no choice
   of filesystem, so such a cell could only run locally. Revisit when a local
   bench needs it.
