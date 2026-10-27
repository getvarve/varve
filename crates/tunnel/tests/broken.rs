//! Broken tunnel detection. This file pauses and stops the SSH container,
//! so it is its own test binary (cargo runs test binaries one at a time)
//! and holds a single test.

mod common;

use std::time::Duration;

use common::*;
use varve_tunnel::{TunnelError, TunnelKey, TunnelManager, TunnelStatus};

/// Brings the SSH container back even when an assertion fails.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = compose(&["unpause", "openssh"]);
        let _ = compose(&["start", "openssh"]);
    }
}

#[tokio::test]
#[ignore = "needs the varve-m1-ssh compose project"]
async fn broken_tunnel_is_detected_and_replaced() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut params = password_params(dir.path());
    params.keepalive = Duration::from_secs(1);
    let manager: TunnelManager = TunnelManager::with_grace(Duration::from_secs(30));
    let key = TunnelKey::new("profile-broken", "device-a");
    let _restore = Restore;

    // 1. A frozen server is detected by keepalives.
    let first = manager.acquire(key.clone(), params.clone()).await?;
    redis_ping(first.local_addr()).await?;
    compose(&["pause", "openssh"])?;
    let status =
        tokio::time::timeout(Duration::from_secs(20), first.tunnel().wait_closed()).await?;
    match &status {
        TunnelStatus::Broken { reason } => assert!(reason.contains("keepalive"), "{reason}"),
        other => panic!("expected Broken, got {other:?}"),
    }
    assert!(!first.is_alive());
    // The lease still points at the port: it stays bound but refuses, so no
    // other local process can take it over.
    assert_bound_but_refusing(first.local_addr()).await?;
    compose(&["unpause", "openssh"])?;
    wait_for_ssh(Duration::from_secs(30)).await?;

    // The broken tunnel is replaced on the next acquire.
    let second = manager.acquire(key.clone(), params.clone()).await?;
    assert_ne!(second.local_addr(), first.local_addr());
    assert!(second.is_alive());
    redis_ping(second.local_addr()).await?;
    assert_bound_but_refusing(first.local_addr()).await?;
    let first_addr = first.local_addr();
    drop(first);
    // The last lease of the broken tunnel is gone: the port is released.
    assert_released(first_addr).await?;

    // 2. A stopped server is detected right away.
    compose(&["stop", "openssh"])?;
    let status =
        tokio::time::timeout(Duration::from_secs(20), second.tunnel().wait_closed()).await?;
    assert!(matches!(status, TunnelStatus::Broken { .. }), "{status:?}");
    assert!(!second.is_alive());
    assert!(matches!(
        second.tunnel().check_health().await,
        Err(TunnelError::Closed { .. })
    ));
    assert_bound_but_refusing(second.local_addr()).await?;

    compose(&["start", "openssh"])?;
    wait_for_ssh(Duration::from_secs(60)).await?;
    let third = manager.acquire(key.clone(), params).await?;
    assert_ne!(third.local_addr(), second.local_addr());
    redis_ping(third.local_addr()).await?;
    let second_addr = second.local_addr();
    assert_bound_but_refusing(second_addr).await?;
    drop(second);
    assert_released(second_addr).await?;
    drop(third);
    manager.close_all().await;
    Ok(())
}
