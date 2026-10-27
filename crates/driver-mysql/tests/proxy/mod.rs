//! A small TCP proxy for the integration tests, to provoke what a real
//! network or a MySQL proxy can do: rewrite the connection id in the
//! handshake, and hold back `KILL QUERY` requests.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// What the proxy changes.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProxyOptions {
    /// Replace the connection id in the server's handshake with this one,
    /// as connection-pooling proxies do.
    pub fake_connection_id: Option<u32>,
    /// Hold back every client packet that contains `KILL QUERY` this long.
    pub kill_delay: Option<Duration>,
    /// Read everything the server sends at once, but hand it to the client
    /// in 8 KiB pieces with this pause after each: the server is done long
    /// before the client has read its results.
    pub slow_delivery: Option<Duration>,
    /// Replace these bytes (same length) in what the client sends, to make
    /// one statement fail on the server while the connection stays fine.
    pub rewrite: Option<(&'static [u8], &'static [u8])>,
    /// Hide session state tracking (`CLIENT_SESSION_TRACK`) in the server's
    /// handshake, like a server or proxy without it.
    pub strip_session_track: bool,
}

/// A running proxy; stops when dropped.
pub struct Proxy {
    /// The local port to connect to.
    pub port: u16,
    task: JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts a proxy on 127.0.0.1 to `upstream` (`host:port`).
pub async fn start(upstream: String, options: ProxyOptions) -> io::Result<Proxy> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let upstream = upstream.clone();
            tokio::spawn(async move {
                if let Err(error) = relay(client, &upstream, options).await {
                    eprintln!("proxy connection ended: {error}");
                }
            });
        }
    });
    Ok(Proxy { port, task })
}

async fn relay(client: TcpStream, upstream: &str, options: ProxyOptions) -> io::Result<()> {
    let server = TcpStream::connect(upstream).await?;
    client.set_nodelay(true)?;
    server.set_nodelay(true)?;
    let (mut client_read, mut client_write) = client.into_split();
    let (mut server_read, mut server_write) = server.into_split();

    let to_client = async {
        // The first packet is the handshake.
        let mut header = [0_u8; 4];
        server_read.read_exact(&mut header).await?;
        let length =
            usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
        let mut payload = vec![0_u8; length];
        server_read.read_exact(&mut payload).await?;
        if let Some(fake) = options.fake_connection_id {
            rewrite_connection_id(&mut payload, fake)?;
        }
        if options.strip_session_track {
            strip_session_track(&mut payload)?;
        }
        client_write.write_all(&header).await?;
        client_write.write_all(&payload).await?;
        match options.slow_delivery {
            None => {
                tokio::io::copy(&mut server_read, &mut client_write).await?;
            }
            Some(pause) => {
                let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
                let read_all = async {
                    let mut buffer = vec![0_u8; 64 * 1024];
                    loop {
                        let read = server_read.read(&mut buffer).await?;
                        if read == 0 || sender.send(buffer[..read].to_vec()).is_err() {
                            break;
                        }
                    }
                    drop(sender);
                    Ok::<_, io::Error>(())
                };
                let deliver = async {
                    while let Some(chunk) = receiver.recv().await {
                        for piece in chunk.chunks(8192) {
                            client_write.write_all(piece).await?;
                            tokio::time::sleep(pause).await;
                        }
                    }
                    Ok::<_, io::Error>(())
                };
                tokio::try_join!(read_all, deliver)?;
            }
        }
        client_write.shutdown().await
    };
    let to_server = async {
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let read = client_read.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            let chunk = &mut buffer[..read];
            if let Some((from, to)) = options.rewrite {
                let mut at = 0;
                while let Some(offset) = chunk[at..]
                    .windows(from.len())
                    .position(|window| window == from)
                {
                    let start = at + offset;
                    chunk[start..start + to.len()].copy_from_slice(to);
                    at = start + from.len();
                }
            }
            let chunk = &*chunk;
            if let Some(delay) = options.kill_delay
                && contains(chunk, b"KILL QUERY")
            {
                tokio::time::sleep(delay).await;
            }
            server_write.write_all(chunk).await?;
        }
        server_write.shutdown().await
    };
    tokio::try_join!(to_client, to_server).map(|_| ())
}

/// Protocol version byte, NUL-terminated server version, then the 4-byte
/// connection id (little endian).
fn rewrite_connection_id(handshake: &mut [u8], fake: u32) -> io::Result<()> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "unexpected handshake");
    if handshake.first() != Some(&10) {
        return Err(invalid());
    }
    let nul = handshake
        .iter()
        .skip(1)
        .position(|byte| *byte == 0)
        .ok_or_else(invalid)?
        + 1;
    let id = handshake.get_mut(nul + 1..nul + 5).ok_or_else(invalid)?;
    id.copy_from_slice(&fake.to_le_bytes());
    Ok(())
}

/// Clears `CLIENT_SESSION_TRACK` (bit 23) in the handshake's capability
/// flags: after the connection id come 8 bytes of auth data, a filler,
/// the lower 2 capability bytes, the character set, 2 status bytes, then
/// the upper 2 capability bytes.
fn strip_session_track(handshake: &mut [u8]) -> io::Result<()> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "unexpected handshake");
    let nul = handshake
        .iter()
        .skip(1)
        .position(|byte| *byte == 0)
        .ok_or_else(invalid)?
        + 1;
    let upper = nul + 1 + 4 + 8 + 1 + 2 + 1 + 2;
    let byte = handshake.get_mut(upper).ok_or_else(invalid)?;
    *byte &= !0x80;
    Ok(())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
