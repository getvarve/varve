//! Writes fresh test certificates for the TLS integration tests into the
//! directory given as the only argument (see `tests/tls.rs`):
//!
//! ```sh
//! cargo run -p varve-driver-postgres --example tls-test-certs -- "$VARVE_PG_TLS_DIR"
//! ```

#[path = "../src/test_certs.rs"]
mod certs;

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let (Some(dir), None) = (args.next(), args.next()) else {
        return Err("usage: tls-test-certs <directory>".into());
    };
    let dir = PathBuf::from(dir);
    certs::generate()?.write_to(&dir)?;
    println!("wrote test certificates to {}", dir.display());
    Ok(())
}
