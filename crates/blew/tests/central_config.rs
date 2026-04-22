#![cfg(feature = "testing")]

use std::time::Duration;

use blew::{Central, CentralConfig};

#[tokio::test]
#[ignore = "requires a real BLE adapter with TCC authorization"]
async fn with_config_accepts_restore_identifier() {
    let config = CentralConfig {
        restore_identifier: Some("com.example.test".into()),
        ..CentralConfig::default()
    };
    let _central = Central::with_config(config).await.unwrap();
}

#[test]
fn central_config_default_has_no_restore_identifier() {
    let c = CentralConfig::default();
    assert!(c.restore_identifier.is_none());
}

#[test]
fn central_config_default_sets_15s_connect_timeout() {
    let c = CentralConfig::default();
    assert_eq!(c.connect_timeout, Some(Duration::from_secs(15)));
}

#[test]
fn central_config_connect_timeout_is_opt_outable() {
    let c = CentralConfig {
        connect_timeout: None,
        ..CentralConfig::default()
    };
    assert!(c.connect_timeout.is_none());
}
