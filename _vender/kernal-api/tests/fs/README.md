# `tests/fs`

Host-neutral filesystem contract tests (`fs::<module>::<test>`), asserted
through the public `kernal_api::platform::fs` facade on every supported
host — never through a concrete platform tree. One linked binary for the
category (AGENTS.md); each file is a module declared by `main.rs`.

Native mechanics that must be tested against the OS live beside the
implementation inside `src/platform_*/` instead.
