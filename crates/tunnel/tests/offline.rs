//! Connection failures that need no SSH server.

use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use varve_secrets::Secret;
use varve_tunnel::{
    HostKeyPolicy, KnownHosts, SshAuth, TimeoutStage, Tunnel, TunnelError, TunnelErrorKind,
    TunnelParams,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn params(dir: &std::path::Path, port: u16) -> TunnelParams {
    let mut p = TunnelParams::new(
        "127.0.0.1",
        "nobody",
        SshAuth::Password(Secret::new("s3cr3t-value".to_owned())),
        "db",
        5432,
        KnownHosts::new(dir.join("known_hosts"), HostKeyPolicy::Strict),
    );
    p.ssh_port = port;
    p.connect_timeout = Duration::from_millis(500);
    p
}

#[tokio::test]
async fn refused_port_is_a_connect_error() -> TestResult {
    let dir = tempfile::tempdir()?;
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        listener.local_addr()?.port()
    };
    match Tunnel::open(params(dir.path(), port)).await {
        Err(e @ TunnelError::Connect { .. }) => {
            assert_eq!(e.kind(), TunnelErrorKind::Connect);
            assert!(!e.to_string().contains("s3cr3t"));
        }
        other => panic!("expected Connect, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn silent_server_times_out() -> TestResult {
    let dir = tempfile::tempdir()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(async move {
        // Accept and say nothing.
        let conn = listener.accept().await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        drop(conn);
    });
    match Tunnel::open(params(dir.path(), port)).await {
        Err(TunnelError::Timeout { stage, after }) => {
            assert_eq!(stage, TimeoutStage::Connect);
            assert_eq!(after, Duration::from_millis(500));
        }
        other => panic!("expected Timeout, got {other:?}"),
    }
    server.abort();
    Ok(())
}

#[tokio::test]
async fn non_ssh_server_is_a_connect_error() -> TestResult {
    let dir = tempfile::tempdir()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(async move {
        if let Ok((mut conn, _)) = listener.accept().await {
            let _ = conn.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            let _ = conn.shutdown().await;
        }
    });
    match Tunnel::open(params(dir.path(), port)).await {
        Err(TunnelError::Connect { reason, .. }) => assert!(!reason.is_empty()),
        other => panic!("expected Connect, got {other:?}"),
    }
    server.abort();
    Ok(())
}
