//! Command configuration: console suppression, process-group setup, and
//! argv[0] identity.

pub use crate::platform_imp::process::command::{
    arg0, configure_parent_death, configure_process_group, supports_process_groups,
    suppress_console,
};
