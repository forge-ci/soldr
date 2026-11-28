//! Network, archive, and async-engine plumbing.
//!
//! One linked test binary per category, not per file: each module below was
//! its own top-level integration test, and each one statically linked this
//! crate's whole graph. Test IDs are now `net::<module>::<test>`.
//! See AGENTS.md for the rule.

mod archive_facade;
mod archive_writers;
mod async_biased_race;
mod async_blocking_channel;
mod async_bounded_channel;
mod async_broadcast;
mod async_fair_race;
mod async_mutex;
mod async_notify_task;
mod async_rw_lock;
mod async_task_local;
mod async_timers_permits;
mod bounded_ipc_connect;
mod http_client;
mod http_server;
mod owner_only_local_ipc;
