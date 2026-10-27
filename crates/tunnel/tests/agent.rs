//! SSH agent authentication against a private `ssh-agent` started by the
//! test. `SSH_AUTH_SOCK` cannot be changed inside this process (that needs
//! `unsafe`), so `agent_auth` runs `agent_auth_child` in a child process of
//! this test binary with the variable set.

mod common;

use std::path::Path;
use std::process::{Child, Command};
use std::time::Duration;

use common::*;
use varve_tunnel::{HostKeyPolicy, SshAuth, Tunnel, TunnelError};

const EXPECT: &str = "VARVE_TUNNEL_AGENT_CHILD_EXPECT";

struct Agent(Child);

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn ssh_add(sock: &Path, args: &[&str]) -> TestResult {
    let out = Command::new("ssh-add")
        .args(args)
        .env("SSH_AUTH_SOCK", sock)
        .output()?;
    if !out.status.success() {
        return Err(format!("ssh-add {args:?}: {}", String::from_utf8_lossy(&out.stderr)).into());
    }
    Ok(())
}

fn run_child(sock: Option<&Path>, expect: &str) -> TestResult {
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args([
        "--exact",
        "agent_auth_child",
        "--include-ignored",
        "--nocapture",
    ])
    .env(EXPECT, expect);
    match sock {
        Some(sock) => cmd.env("SSH_AUTH_SOCK", sock),
        None => cmd.env_remove("SSH_AUTH_SOCK"),
    };
    let out = cmd.output()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || !stdout.contains("1 passed") {
        return Err(format!(
            "child ({expect}) failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the varve-m1-ssh compose project"]
async fn agent_auth() -> TestResult {
    // Unix socket paths are limited to about 100 bytes; keep it short.
    let dir = tempfile::Builder::new().prefix("vt").tempdir_in("/tmp")?;
    let sock = dir.path().join("a.sock");
    let _agent = Agent(
        Command::new("ssh-agent")
            .arg("-D")
            .arg("-a")
            .arg(&sock)
            .stdout(std::process::Stdio::null())
            .spawn()?,
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !sock.exists() {
        if std::time::Instant::now() > deadline {
            return Err("ssh-agent did not create its socket".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let (ed, ed_pub) = ssh_keygen(dir.path(), "ed", &["-t", "ed25519", "-N", ""])?;
    let (rsa, rsa_pub) = ssh_keygen(dir.path(), "rsa", &["-t", "rsa", "-b", "3072", "-N", ""])?;
    authorize(&ed_pub).await?;
    authorize(&rsa_pub).await?;

    run_child(Some(&sock), "no-keys")?;
    ssh_add(&sock, &[ed.to_str().ok_or("path")?])?;
    run_child(Some(&sock), "ok")?;
    ssh_add(&sock, &["-D"])?;
    // RSA through the agent must sign with rsa-sha2-256/512.
    ssh_add(&sock, &[rsa.to_str().ok_or("path")?])?;
    run_child(Some(&sock), "ok")?;
    run_child(None, "no-agent")?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs the varve-m1-ssh compose project"]
async fn agent_auth_child() -> TestResult {
    let Ok(expect) = std::env::var(EXPECT) else {
        eprintln!("agent_auth_child only does something when started by agent_auth");
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let result = Tunnel::open(params(dir.path(), SshAuth::Agent, HostKeyPolicy::AcceptNew)).await;
    match (expect.as_str(), result) {
        ("ok", Ok(tunnel)) => {
            redis_ping(tunnel.local_addr()).await?;
            tunnel.close().await;
        }
        ("no-keys", Err(TunnelError::Agent { reason })) => {
            assert!(reason.contains("no keys"), "{reason}");
        }
        ("no-agent", Err(TunnelError::Agent { reason })) => {
            assert!(reason.contains("SSH_AUTH_SOCK"), "{reason}");
        }
        (expect, other) => panic!("expected {expect}, got {other:?}"),
    }
    Ok(())
}
