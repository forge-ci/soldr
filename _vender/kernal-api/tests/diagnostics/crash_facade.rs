//! Consumer-style contract for the crash facade's public surface.
//!
//! This fixture is a separate crate, so it can only reach what a real client
//! can. An in-crate test can name `CrashMetadata` through the private `use`
//! in `crash`, so it cannot notice the type being unreachable from outside.
//!
//! zccache hit exactly that: `kernal_api::crash::install` is the entry point,
//! but naming its argument meant reaching into `crash::spool`.

#![cfg(feature = "crash")]

use kernal_api::crash::{CrashMetadata, CrashPolicy, CrashSamplerConfig, InstallError};
use std::time::Duration;

/// `install`'s argument type is nameable from `install`'s own module.
#[test]
fn a_client_can_name_the_metadata_install_asks_for() {
    let metadata = CrashMetadata {
        app_class: "build-cache".to_string(),
        app_name: "zccache".to_string(),
        app_version: "1.13.22".to_string(),
        instance_name: String::new(),
        creation_time_ms: 0,
        cwd: String::new(),
    };

    assert_eq!(metadata.app_name, "zccache");

    // The same type, reached the long way. If these ever diverge, one of the
    // two paths has been repointed at something else.
    let via_spool: kernal_api::crash::spool::CrashMetadata = metadata.clone();
    assert_eq!(via_spool, metadata);

    // Named here so the policy a client passes alongside it stays reachable
    // from the same place.
    assert_eq!(CrashPolicy::default(), CrashPolicy::On);

    let defaults = CrashSamplerConfig::default();
    assert_eq!(defaults.interval, Duration::from_millis(100));
    assert_eq!(defaults.max_interval, Duration::from_secs(1));
    let inert = kernal_api::crash::install_with_sampler_config(
        CrashPolicy::Off,
        metadata.clone(),
        CrashSamplerConfig {
            interval: Duration::from_millis(250),
            max_interval: Duration::from_secs(2),
        },
    )
    .expect("a client can select the cadence");
    assert!(!inert.is_armed());
    assert!(matches!(
        kernal_api::crash::install_with_sampler_config(
            CrashPolicy::On,
            metadata,
            CrashSamplerConfig {
                interval: Duration::ZERO,
                max_interval: Duration::from_secs(1),
            },
        ),
        Err(InstallError::InvalidSamplerConfig)
    ));
}
