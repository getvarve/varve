//! Integration tests against the OpenSSH container. Start it with
//! `docker compose -p varve-m1-ssh -f crates/tunnel/tests/docker-compose.yml up -d --wait`.

mod common;

use std::time::Duration;

use common::*;
use tokio::io::AsyncReadExt;
use varve_secrets::Secret;
use varve_tunnel::{
    AuthMethod, HostKeyPolicy, KnownHosts, SshAuth, Tunnel, TunnelError, TunnelKey, TunnelManager,
    TunnelStatus, fingerprint_openssh,
};

#[tokio::test]
#[ignore = "needs the varve-m1-ssh compose project"]
async fn password_auth_forwards_both_directions() -> TestResult {
    let dir = tempfile::tempdir()?;
    let tunnel = Tunnel::open(password_params(dir.path())).await?;
    assert!(tunnel.is_alive());
    assert!(tunnel.local_addr().ip().is_loopback());
    redis_ping(tunnel.local_addr()).await?;
    // 4 MiB each way: more than the SSH window, so flow control is exercised.
    redis_echo(tunnel.local_addr(), &payload(4 * 1024 * 1024, 7)).await?;
    tunnel.check_health().await?;
    tunnel.probe_target().await?;
    tunnel.close().await;
    assert_eq!(tunnel.status(), TunnelStatus::Closed);
    assert!(!tunnel.is_alive());
    assert!(matches!(
        tunnel.check_health().await,
        Err(TunnelError::Closed { .. })
    ));
    // Closed, but still owned: the port stays bound and refuses, so no
    // other local process can take it over.
    let addr = tunnel.local_addr();
    assert_bound_but_refusing(addr).await?;
    assert_bound_but_refusing(addr).await?;
    // Dropping the tunnel releases the port.
    drop(tunnel);
    assert_released(addr).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the varve-m1-ssh compose project"]
async fn key_file_auth_without_passphrase() -> TestResult {
    let dir = tempfile::tempdir()?;
    let (path, public) = write_ed25519_key(dir.path(), "id_ed25519", None)?;
    authorize(&public).await?;
    let auth = SshAuth::KeyFile {
        path,
        passphrase: None,
    };
    let tunnel = Tunnel::open(params(dir.path(), auth, HostKeyPolicy::AcceptNew)).await?;
    redis_ping(tunnel.local_addr()).await?;
    tunnel.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the varve-m1-ssh compose project"]
async fn key_file_auth_with_passphrase() -> TestResult {
    let dir = tempfile::tempdir()?;
    let (path, public) = write_ed25519_key(dir.path(), "id_ed25519_pw", Some("tunnel pass"))?;
    authorize(&public).await?;
    let auth = SshAuth::KeyFile {
        path: path.clone(),
        passphrase: Some(Secret::new("tunnel pass".to_owned())),
    };
    let tunnel = Tunnel::open(params(dir.path(), auth, HostKeyPolicy::AcceptNew)).await?;
    redis_ping(tunnel.local_addr()).await?;
    tunnel.close().await;

    let wrong = SshAuth::KeyFile {
        path,
        passphrase: Some(Secret::new("not it".to_owned())),
    };
    match Tunnel::open(params(dir.path(), wrong, HostKeyPolicy::AcceptNew)).await {
        Err(e @ TunnelError::KeyFile { .. }) => assert!(!e.to_string().contains("not it")),
        other => panic!("expected KeyFile error, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the varve-m1-ssh compose project"]
async fn rsa_and_pem_key_files() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cases: &[(&str, &[&str], Option<&str>)] = &[
        // RSA must sign with rsa-sha2-256/512: OpenSSH 10 refuses ssh-rsa.
        ("id_rsa", &["-t", "rsa", "-b", "3072", "-N", ""], None),
        (
            "id_rsa_pem",
            &["-t", "rsa", "-b", "2048", "-m", "PEM", "-N", "pem pw"],
            Some("pem pw"),
        ),
        (
            "id_ecdsa_pem",
            &["-t", "ecdsa", "-b", "384", "-m", "PEM", "-N", ""],
            None,
        ),
        (
            "id_ecdsa_pkcs8",
            &["-t", "ecdsa", "-b", "256", "-m", "PKCS8", "-N", ""],
            None,
        ),
    ];
    for (name, args, pass) in cases {
        let (path, public) = ssh_keygen(dir.path(), name, args)?;
        authorize(&public).await?;
        let auth = SshAuth::KeyFile {
            path,
            passphrase: pass.map(|p| Secret::new(p.to_owned())),
        };
        let tunnel = Tunnel::open(params(dir.path(), auth, HostKeyPolicy::AcceptNew))
            .await
            .map_err(|e| format!("{name}: {e}"))?;
        redis_ping(tunnel.local_addr()).await?;
        tunnel.close().await;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the varve-m1-ssh compose project"]
async fn wrong_password_is_an_auth_error() -> TestResult {
    let dir = tempfile::tempdir()?;
    let auth = SshAuth::Password(Secret::new("definitely-wrong-pw".to_owned()));
    match Tunnel::open(params(dir.path(), auth, HostKeyPolicy::AcceptNew)).await {
        Err(e @ TunnelError::Auth { .. }) => {
            let msg = e.to_string();
            assert!(!msg.contains("definitely-wrong-pw"), "{msg}");
            let TunnelError::Auth {
                method, accepted, ..
            } = e
            else {
                unreachable!()
            };
            assert_eq!(method, AuthMethod::Password);
            assert!(accepted.iter().any(|m| m == "password"), "{accepted:?}");
            assert!(accepted.iter().any(|m| m == "publickey"), "{accepted:?}");
        }
        other => panic!("expected Auth error, got {other:?}"),
    }

    // A key the server does not know.
    let (path, _) = write_ed25519_key(dir.path(), "unknown_key", None)?;
    let auth = SshAuth::KeyFile {
        path,
        passphrase: None,
    };
    match Tunnel::open(params(dir.path(), auth, HostKeyPolicy::AcceptNew)).await {
        Err(TunnelError::Auth { method, .. }) => assert_eq!(method, AuthMethod::KeyFile),
        other => panic!("expected Auth error, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the varve-m1-ssh compose project"]
async fn strict_unknown_then_accept_new_then_strict() -> TestResult {
    let dir = tempfile::tempdir()?;
    let auth = || SshAuth::Password(ssh_password());

    let presented = match Tunnel::open(params(dir.path(), auth(), HostKeyPolicy::Strict)).await {
        Err(TunnelError::HostKeyUnknown {
            fingerprint,
            presented_key,
            port,
            ..
        }) => {
            assert_eq!(port, ssh_port());
            assert!(fingerprint.starts_with("SHA256:"), "{fingerprint}");
            assert_eq!(fingerprint_openssh(&presented_key)?, fingerprint);
            fingerprint
        }
        other => panic!("expected HostKeyUnknown, got {other:?}"),
    };
    assert!(!dir.path().join("known_hosts").exists());

    let tunnel = Tunnel::open(params(dir.path(), auth(), HostKeyPolicy::AcceptNew)).await?;
    tunnel.close().await;
    let text = std::fs::read_to_string(dir.path().join("known_hosts"))?;
    let expected_name = format!("[{}]:{} ", ssh_host(), ssh_port());
    assert!(text.starts_with(&expected_name), "{text}");
    assert_eq!(text.lines().count(), 1, "{text}");
    let recorded = text.split_once(' ').map(|(_, k)| k).unwrap_or_default();
    assert_eq!(fingerprint_openssh(recorded)?, presented);

    let tunnel = Tunnel::open(params(dir.path(), auth(), HostKeyPolicy::Strict)).await?;
    redis_ping(tunnel.local_addr()).await?;
    tunnel.close().await;
    // A second AcceptNew connect does not add a duplicate.
    Tunnel::open(params(dir.path(), auth(), HostKeyPolicy::AcceptNew))
        .await?
        .close()
        .await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("known_hosts"))?
            .lines()
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs the varve-m1-ssh compose project"]
async fn changed_host_key_reports_both_fingerprints() -> TestResult {
    let dir = tempfile::tempdir()?;
    // Learn the real key in one file.
    let real_dir = tempfile::tempdir()?;
    Tunnel::open(password_params(real_dir.path()))
        .await?
        .close()
        .await;
    let real_text = std::fs::read_to_string(real_dir.path().join("known_hosts"))?;
    let real_key = real_text
        .split_once(' ')
        .map(|(_, k)| k.trim())
        .unwrap_or_default();
    let real_fp = fingerprint_openssh(real_key)?;

    // Record a different key of the same type for the server.
    let (_, impostor) = write_ed25519_key(dir.path(), "impostor", None)?;
    let impostor_fp = fingerprint_openssh(&impostor)?;
    let known = KnownHosts::new(dir.path().join("known_hosts"), HostKeyPolicy::AcceptNew);
    known.add(
        &ssh_host(),
        ssh_port(),
        &varve_tunnel::parse_public_key(&impostor)?,
    )?;

    for policy in [HostKeyPolicy::AcceptNew, HostKeyPolicy::Strict] {
        let p = params(dir.path(), SshAuth::Password(ssh_password()), policy);
        match Tunnel::open(p).await {
            Err(TunnelError::HostKeyChanged(change)) => {
                assert_eq!(change.expected_fingerprint, impostor_fp);
                assert_eq!(change.actual_fingerprint, real_fp);
                assert_eq!(change.line, 1);
                assert_eq!(change.key_type, "ssh-ed25519");
            }
            other => panic!("expected HostKeyChanged, got {other:?}"),
        }
    }

    // After the user confirms, trusting the presented key fixes it.
    known.trust(
        &ssh_host(),
        ssh_port(),
        &varve_tunnel::parse_public_key(real_key)?,
    )?;
    let p = params(
        dir.path(),
        SshAuth::Password(ssh_password()),
        HostKeyPolicy::Strict,
    );
    Tunnel::open(p).await?.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the varve-m1-ssh compose project"]
async fn many_parallel_connections_over_one_tunnel() -> TestResult {
    let dir = tempfile::tempdir()?;
    let tunnel = Tunnel::open(password_params(dir.path())).await?;
    let addr = tunnel.local_addr();
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..64u8 {
        tasks.spawn(async move {
            redis_ping(addr).await?;
            redis_echo(addr, &payload(64 * 1024 + usize::from(i), i)).await
        });
    }
    let mut done = 0;
    while let Some(result) = tasks.join_next().await {
        result??;
        done += 1;
    }
    assert_eq!(done, 64);
    assert!(tunnel.is_alive());
    tunnel.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the varve-m1-ssh compose project"]
async fn refused_target_is_a_forward_error() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut p = password_params(dir.path());
    p.target_port = 6390; // nothing listens there
    let tunnel = Tunnel::open(p).await?;
    match tunnel.probe_target().await {
        Err(TunnelError::Forward {
            target_port,
            reason,
            ..
        }) => {
            assert_eq!(target_port, 6390);
            assert!(reason.contains("refused"), "{reason}");
        }
        other => panic!("expected Forward error, got {other:?}"),
    }
    // A driver connection is accepted locally and closed right away.
    let mut stream = tokio::net::TcpStream::connect(tunnel.local_addr()).await?;
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await?;
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    assert!(matches!(
        tunnel.last_forward_error(),
        Some(TunnelError::Forward { .. })
    ));
    // The session itself is fine.
    assert!(tunnel.is_alive());
    tunnel.check_health().await?;
    tunnel.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the varve-m1-ssh compose project"]
async fn manager_reuses_and_closes_after_grace() -> TestResult {
    let dir = tempfile::tempdir()?;
    let manager: TunnelManager = TunnelManager::with_grace(Duration::from_millis(1500));
    let key = TunnelKey::new("profile-1", "device-a");

    // Concurrent acquires open a single tunnel.
    let (a, b, c) = tokio::join!(
        manager.acquire(key.clone(), password_params(dir.path())),
        manager.acquire(key.clone(), password_params(dir.path())),
        manager.acquire(key.clone(), password_params(dir.path())),
    );
    let (a, b, c) = (a?, b?, c?);
    assert_eq!(a.local_addr(), b.local_addr());
    assert_eq!(a.local_addr(), c.local_addr());
    assert_eq!(manager.lease_count(&key), 3);
    // Exactly one of them opened it; the others shared it.
    assert_eq!(
        [a.opened(), b.opened(), c.opened()]
            .iter()
            .filter(|o| **o)
            .count(),
        1
    );
    redis_ping(a.local_addr()).await?;
    let addr = a.local_addr();

    // Another key gets its own tunnel.
    let other_key = TunnelKey::new("profile-1", "device-b");
    let other = manager
        .acquire(other_key.clone(), password_params(dir.path()))
        .await?;
    assert_ne!(other.local_addr(), addr);
    assert!(other.opened());
    drop(other);

    drop((a, b, c));
    assert_eq!(manager.lease_count(&key), 0);
    assert_eq!(manager.status(&key), Some(TunnelStatus::Open));
    // Without a lease the port does not forward, though the session is kept.
    assert_not_forwarding(addr).await?;

    // A quick reconnect within the grace period reuses the tunnel.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let again = manager
        .acquire(key.clone(), password_params(dir.path()))
        .await?;
    assert_eq!(again.local_addr(), addr);
    assert!(!again.opened(), "reused, not opened again");
    redis_ping(again.local_addr()).await?;
    drop(again);

    // The earlier grace timer must not close the re-acquired tunnel early:
    // 1.2 s after the second release (2.2 s after the first) it is still open.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(manager.status(&key), Some(TunnelStatus::Open));
    assert_not_forwarding(addr).await?;

    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(manager.status(&key), None);
    assert_eq!(manager.status(&other_key), None);
    // Give the close task a moment, then the port refuses connections.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_released(addr).await?;

    // A new acquire opens a fresh tunnel.
    let fresh = manager
        .acquire(key.clone(), password_params(dir.path()))
        .await?;
    redis_ping(fresh.local_addr()).await?;
    drop(fresh);
    manager.close_all().await;
    assert_eq!(manager.status(&key), None);
    Ok(())
}

#[tokio::test]
#[ignore = "needs the varve-m1-ssh compose project"]
async fn manager_replaces_tunnel_when_endpoint_changes() -> TestResult {
    let dir = tempfile::tempdir()?;
    let manager: TunnelManager = TunnelManager::with_grace(Duration::from_secs(30));
    let key = TunnelKey::new("profile-2", "device-a");
    let first = manager
        .acquire(key.clone(), password_params(dir.path()))
        .await?;
    let mut edited = password_params(dir.path());
    edited.target_port = 6390;
    let second = manager.acquire(key.clone(), edited).await?;
    assert_ne!(first.local_addr(), second.local_addr());
    // The old tunnel keeps serving its lease until it is dropped.
    assert!(first.is_alive());
    redis_ping(first.local_addr()).await?;
    let old_addr = first.local_addr();
    drop(first);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_released(old_addr).await?;
    manager.close(&key).await;
    assert!(!second.is_alive());
    // Closed while a lease still points at the port: bound but refusing
    // until that lease is dropped.
    let second_addr = second.local_addr();
    assert_bound_but_refusing(second_addr).await?;
    drop(second);
    assert_released(second_addr).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the varve-m1-ssh compose project"]
async fn manager_forwards_only_while_a_lease_is_held() -> TestResult {
    let dir = tempfile::tempdir()?;
    let manager: TunnelManager = TunnelManager::with_grace(Duration::from_secs(30));
    let key = TunnelKey::new("profile-lease", "device-a");

    let lease = manager
        .acquire(key.clone(), password_params(dir.path()))
        .await?;
    let addr = lease.local_addr();
    redis_ping(addr).await?;
    // A connection forwarded while the lease is held keeps working after
    // the release; only new connections are refused.
    let mut open = tokio::net::TcpStream::connect(addr).await?;
    // Answered, so it was accepted and forwarded before the release.
    redis_ping_on(&mut open).await?;
    drop(lease);

    assert_eq!(manager.status(&key), Some(TunnelStatus::Open));
    for _ in 0..3 {
        assert_not_forwarding(addr).await?;
    }
    redis_ping_on(&mut open).await?;
    // A refused connection is not a forwarding failure.
    assert!(manager.status(&key) == Some(TunnelStatus::Open));

    // Re-acquiring within the grace period reuses the tunnel and forwards again.
    let again = manager
        .acquire(key.clone(), password_params(dir.path()))
        .await?;
    assert_eq!(again.local_addr(), addr);
    redis_ping(addr).await?;

    // Two leases: releasing one keeps forwarding.
    let second = manager
        .acquire(key.clone(), password_params(dir.path()))
        .await?;
    drop(again);
    redis_ping(addr).await?;
    drop(second);
    assert_not_forwarding(addr).await?;

    manager.close_all().await;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the varve-m1-ssh compose project"]
async fn manager_opens_a_new_tunnel_for_a_stricter_policy() -> TestResult {
    let dir = tempfile::tempdir()?;
    let manager: TunnelManager = TunnelManager::with_grace(Duration::from_secs(30));
    let key = TunnelKey::new("profile-policy", "device-a");

    // AcceptNew records the host key; the user then tightens the policy.
    let first = manager
        .acquire(key.clone(), password_params(dir.path()))
        .await?;
    let strict = params(
        dir.path(),
        SshAuth::Password(ssh_password()),
        HostKeyPolicy::Strict,
    );
    let second = manager.acquire(key.clone(), strict.clone()).await?;
    assert_ne!(first.local_addr(), second.local_addr());
    assert_eq!(manager.lease_count(&key), 1);
    redis_ping(second.local_addr()).await?;

    // The old tunnel keeps serving its lease, then is released normally.
    redis_ping(first.local_addr()).await?;
    let old_addr = first.local_addr();
    drop(first);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_released(old_addr).await?;
    assert!(second.is_alive());

    // The same strict parameters reuse the new tunnel.
    let third = manager.acquire(key.clone(), strict).await?;
    assert_eq!(third.local_addr(), second.local_addr());
    assert_eq!(manager.lease_count(&key), 2);

    // Another key file opens a new tunnel too.
    let (path, public) = write_ed25519_key(dir.path(), "id_policy", None)?;
    authorize(&public).await?;
    let key_file = params(
        dir.path(),
        SshAuth::KeyFile {
            path,
            passphrase: None,
        },
        HostKeyPolicy::Strict,
    );
    let fourth = manager.acquire(key.clone(), key_file).await?;
    assert_ne!(fourth.local_addr(), second.local_addr());
    redis_ping(fourth.local_addr()).await?;
    let replaced = second.local_addr();
    drop((second, third));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_released(replaced).await?;

    drop(fourth);
    manager.close_all().await;
    Ok(())
}

/// When the caller drops a connect in progress (an outer timeout, a
/// `select!`, an aborted task), the host key check that is still running
/// in the session task must not record the key under AcceptNew.
#[tokio::test]
#[ignore = "needs the varve-m1-ssh compose project"]
async fn dropped_connect_does_not_record_the_host_key() -> TestResult {
    let dir = tempfile::tempdir()?;
    let known_hosts = dir.path().join("known_hosts");
    // Another process holds the lock file, so recording the key waits.
    let held = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.path().join("known_hosts.lock"))?;
    held.lock()?;

    let params = password_params(dir.path());
    let open = tokio::spawn(Tunnel::open(params.clone()));
    // Long enough to reach the host key check, far below the lock wait.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!open.is_finished(), "the connect did not wait for the lock");
    open.abort();
    assert!(open.await.is_err_and(|e| e.is_cancelled()));
    held.unlock()?;

    // Give the check time to take the lock and decide.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let text = std::fs::read_to_string(&known_hosts).unwrap_or_default();
    assert!(
        text.trim().is_empty(),
        "an abandoned connect recorded a key: {text}"
    );

    // Dropping a TunnelManager::acquire future behaves the same.
    held.lock()?;
    let manager: TunnelManager = TunnelManager::with_grace(Duration::ZERO);
    let acquire = manager.acquire(TunnelKey::new("dropped", "device"), params.clone());
    assert!(
        tokio::time::timeout(Duration::from_millis(1500), acquire)
            .await
            .is_err()
    );
    held.unlock()?;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let text = std::fs::read_to_string(&known_hosts).unwrap_or_default();
    assert!(
        text.trim().is_empty(),
        "an abandoned acquire recorded a key: {text}"
    );

    // A connect that is not given up records it.
    Tunnel::open(params).await?.close().await;
    assert_eq!(std::fs::read_to_string(&known_hosts)?.lines().count(), 1);
    Ok(())
}
