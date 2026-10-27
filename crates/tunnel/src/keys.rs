//! Private key loading and fingerprints.

use std::io::Read;
use std::path::Path;

use russh::keys::ssh_key;
use russh::keys::{HashAlg, PrivateKey, PublicKey};
use varve_secrets::Secret;
use zeroize::Zeroizing;

use crate::error::{KeyFileProblem, TunnelError};
use crate::pkcs8_ec::Rewrite;

/// Largest key file that is read. Real keys are a few kilobytes.
const MAX_KEY_FILE_BYTES: u64 = 1024 * 1024;

/// SHA256 fingerprint of a public key in OpenSSH style, for example
/// `SHA256:uNiVztksCsDhcc0u9e8BujQXVUpKZIDTMczCvj3tD2s`.
#[must_use]
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// SHA256 fingerprint of a public key given in OpenSSH format
/// (`ssh-ed25519 AAAA... comment`), as found in `.pub` files,
/// `authorized_keys` and the `presented_key` of host key errors.
///
/// # Errors
/// Returns [`TunnelError::InvalidPublicKey`] when the text is not a public key.
pub fn fingerprint_openssh(public_key: &str) -> Result<String, TunnelError> {
    parse_public_key(public_key).map(|k| fingerprint(&k))
}

/// SHA256 fingerprint of the public half of a private key file.
///
/// # Errors
/// Returns [`TunnelError::KeyFile`] when the file cannot be loaded.
pub fn key_file_fingerprint(
    path: &Path,
    passphrase: Option<&Secret>,
) -> Result<String, TunnelError> {
    load_private_key(path, passphrase).map(|k| fingerprint(k.public_key()))
}

/// Parses a public key in OpenSSH format (`<type> <base64> [comment]`).
///
/// # Errors
/// Returns [`TunnelError::InvalidPublicKey`] when the text is not a public key.
pub fn parse_public_key(public_key: &str) -> Result<PublicKey, TunnelError> {
    PublicKey::from_openssh(public_key.trim()).map_err(|_| TunnelError::InvalidPublicKey)
}

/// Loads a private key file.
///
/// Supported formats: OpenSSH (`BEGIN OPENSSH PRIVATE KEY`, plain or with a
/// passphrase), PKCS#1 RSA (`BEGIN RSA PRIVATE KEY`, plain or with the legacy
/// `Proc-Type: 4,ENCRYPTED` AES-128-CBC header written by `ssh-keygen -m PEM`),
/// PKCS#8 (`BEGIN PRIVATE KEY` and `BEGIN ENCRYPTED PRIVATE KEY`) and SEC1
/// (`BEGIN EC PRIVATE KEY`). Key types: Ed25519, ECDSA (P-256, P-384,
/// P-521) and RSA.
///
/// A passphrase given for an unencrypted key is ignored.
///
/// # Errors
/// Returns [`TunnelError::KeyFile`] describing the problem. The error never
/// contains the key or the passphrase.
pub fn load_private_key(
    path: &Path,
    passphrase: Option<&Secret>,
) -> Result<PrivateKey, TunnelError> {
    let fail = |problem| TunnelError::KeyFile {
        path: path.to_path_buf(),
        problem,
    };
    let text = read_key_file(path).map_err(fail)?;
    decode_private_key(&text, passphrase).map_err(fail)
}

fn read_key_file(path: &Path) -> Result<Zeroizing<String>, KeyFileProblem> {
    let file = std::fs::File::open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => KeyFileProblem::NotFound,
        _ => KeyFileProblem::Unreadable(e.to_string()),
    })?;
    let meta = file
        .metadata()
        .map_err(|e| KeyFileProblem::Unreadable(e.to_string()))?;
    if !meta.is_file() {
        return Err(KeyFileProblem::Unreadable("not a regular file".to_owned()));
    }
    if meta.len() > MAX_KEY_FILE_BYTES {
        return Err(KeyFileProblem::InvalidFormat);
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_KEY_FILE_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|e| KeyFileProblem::Unreadable(e.to_string()))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| KeyFileProblem::InvalidFormat)?;
    Ok(Zeroizing::new(text.to_owned()))
}

/// Decodes a private key from its text.
pub(crate) fn decode_private_key(
    text: &str,
    passphrase: Option<&Secret>,
) -> Result<PrivateKey, KeyFileProblem> {
    let encrypted = looks_encrypted(text);
    // A passphrase is only handed to the decoder when the key needs one, so
    // that an unencrypted key never fails because of a stray passphrase.
    let password = if encrypted {
        match passphrase {
            Some(p) => Some(p.expose()),
            None => return Err(KeyFileProblem::PassphraseRequired),
        }
    } else {
        None
    };
    match russh::keys::decode_secret_key(text, password) {
        Ok(key) => Ok(key),
        Err(e) => match crate::pkcs8_ec::rewrite(text, password) {
            Rewrite::Sec1(sec1) => russh::keys::decode_secret_key(&sec1, None)
                .map_err(|e| classify_decode_error(&e, false)),
            Rewrite::WrongPassphrase => Err(KeyFileProblem::WrongPassphrase),
            Rewrite::NotApplicable => Err(classify_decode_error(&e, encrypted)),
        },
    }
}

fn classify_decode_error(error: &russh::keys::Error, encrypted: bool) -> KeyFileProblem {
    use russh::keys::Error as E;
    match error {
        E::KeyIsEncrypted | E::SshKey(ssh_key::Error::Encrypted) => {
            KeyFileProblem::PassphraseRequired
        }
        E::UnsupportedKeyType {
            key_type_string, ..
        } => KeyFileProblem::UnsupportedKeyType(key_type_string.clone()),
        E::SshKey(ssh_key::Error::AlgorithmUnknown) => {
            KeyFileProblem::UnsupportedKeyType("unknown".to_owned())
        }
        E::SshKey(ssh_key::Error::AlgorithmUnsupported { algorithm }) => {
            KeyFileProblem::UnsupportedKeyType(algorithm.to_string())
        }
        E::UnknownAlgorithm(oid) => KeyFileProblem::UnsupportedKeyType(oid.to_string()),
        // Decryption of every supported encrypted format fails with a
        // padding, checksum or structure error when the passphrase is wrong.
        _ if encrypted => KeyFileProblem::WrongPassphrase,
        _ => KeyFileProblem::InvalidFormat,
    }
}

/// Whether a key file needs a passphrase.
fn looks_encrypted(text: &str) -> bool {
    if text.contains("-----BEGIN OPENSSH PRIVATE KEY-----") {
        return PrivateKey::from_openssh(text.trim()).is_ok_and(|k| k.is_encrypted());
    }
    text.contains("-----BEGIN ENCRYPTED PRIVATE KEY-----")
        || text
            .lines()
            .any(|l| l.trim_start().starts_with("Proc-Type:") && l.contains("ENCRYPTED"))
        || text.trim_start().starts_with("PuTTY-User-Key-File-")
            && text
                .lines()
                .any(|l| l.starts_with("Encryption:") && !l.trim_end().ends_with("none"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::ssh_key::LineEnding;
    use russh::keys::{Algorithm, EcdsaCurve};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn random_key(alg: Algorithm) -> Result<PrivateKey, Box<dyn std::error::Error>> {
        Ok(PrivateKey::random(&mut rand::rng(), alg)?)
    }

    fn write(dir: &Path, name: &str, text: &str) -> Result<std::path::PathBuf, std::io::Error> {
        let path = dir.join(name);
        std::fs::write(&path, text)?;
        Ok(path)
    }

    #[test]
    fn loads_openssh_keys_with_and_without_passphrase() -> TestResult {
        let dir = tempfile::tempdir()?;
        let algs = [
            Algorithm::Ed25519,
            Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP256,
            },
            Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP384,
            },
            Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP521,
            },
        ];
        for (i, alg) in algs.into_iter().enumerate() {
            let key = random_key(alg)?;
            let plain = write(
                dir.path(),
                &format!("plain{i}"),
                &key.to_openssh(LineEnding::LF)?,
            )?;
            let loaded = load_private_key(&plain, None)?;
            assert_eq!(loaded.public_key(), key.public_key());
            // A stray passphrase does not break an unencrypted key.
            let stray = Secret::new("unused".to_owned());
            assert_eq!(
                load_private_key(&plain, Some(&stray))?.public_key(),
                key.public_key()
            );

            let encrypted = key.encrypt(&mut rand::rng(), "correct horse")?;
            let enc = write(
                dir.path(),
                &format!("enc{i}"),
                &encrypted.to_openssh(LineEnding::LF)?,
            )?;
            let good = Secret::new("correct horse".to_owned());
            assert_eq!(
                load_private_key(&enc, Some(&good))?.public_key(),
                key.public_key()
            );

            match load_private_key(&enc, None) {
                Err(TunnelError::KeyFile {
                    problem: KeyFileProblem::PassphraseRequired,
                    ..
                }) => {}
                other => panic!("expected PassphraseRequired, got {other:?}"),
            }
            let bad = Secret::new("wrong".to_owned());
            match load_private_key(&enc, Some(&bad)) {
                Err(
                    e @ TunnelError::KeyFile {
                        problem: KeyFileProblem::WrongPassphrase,
                        ..
                    },
                ) => {
                    let msg = e.to_string();
                    assert!(
                        !msg.contains("wrong"),
                        "message leaks the passphrase: {msg}"
                    );
                    assert!(!msg.contains("PRIVATE KEY"), "message leaks the key: {msg}");
                }
                other => panic!("expected WrongPassphrase, got {other:?}"),
            }
        }
        Ok(())
    }

    #[test]
    fn missing_and_garbage_files() -> TestResult {
        let dir = tempfile::tempdir()?;
        match load_private_key(&dir.path().join("nope"), None) {
            Err(TunnelError::KeyFile {
                problem: KeyFileProblem::NotFound,
                ..
            }) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
        let garbage = write(dir.path(), "garbage", "hello\n")?;
        match load_private_key(&garbage, None) {
            Err(TunnelError::KeyFile {
                problem: KeyFileProblem::InvalidFormat,
                ..
            }) => {}
            other => panic!("expected InvalidFormat, got {other:?}"),
        }
        match load_private_key(dir.path(), None) {
            Err(TunnelError::KeyFile {
                problem: KeyFileProblem::Unreadable(_),
                ..
            }) => {}
            other => panic!("expected Unreadable, got {other:?}"),
        }
        Ok(())
    }

    /// Runs `ssh-keygen` to produce keys in the formats users actually have
    /// on disk (RSA, legacy PEM). The keys only exist in a temp directory.
    fn ssh_keygen(
        dir: &Path,
        name: &str,
        args: &[&str],
    ) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
        let path = dir.join(name);
        let out = std::process::Command::new("ssh-keygen")
            .args(args)
            .arg("-q")
            .arg("-C")
            .arg("varve-test")
            .arg("-f")
            .arg(&path)
            .output()
            .map_err(|e| format!("ssh-keygen is needed for this test: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "ssh-keygen failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )
            .into());
        }
        Ok(path)
    }

    fn pub_fingerprint(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
        let text = std::fs::read_to_string(path.with_extension("pub"))?;
        Ok(fingerprint_openssh(&text)?)
    }

    #[test]
    fn loads_rsa_and_pem_keys_from_ssh_keygen() -> TestResult {
        let dir = tempfile::tempdir()?;
        let cases: &[(&str, &[&str], Option<&str>)] = &[
            ("rsa_openssh", &["-t", "rsa", "-b", "2048", "-N", ""], None),
            (
                "rsa_openssh_pw",
                &["-t", "rsa", "-b", "2048", "-N", "pw rsa"],
                Some("pw rsa"),
            ),
            (
                "rsa_pem",
                &["-t", "rsa", "-b", "2048", "-m", "PEM", "-N", ""],
                None,
            ),
            (
                "rsa_pem_pw",
                &["-t", "rsa", "-b", "2048", "-m", "PEM", "-N", "pw pem"],
                Some("pw pem"),
            ),
            (
                "rsa_pkcs8",
                &["-t", "rsa", "-b", "2048", "-m", "PKCS8", "-N", ""],
                None,
            ),
            (
                "ec_pem",
                &["-t", "ecdsa", "-b", "256", "-m", "PEM", "-N", ""],
                None,
            ),
            (
                "ec384_pem",
                &["-t", "ecdsa", "-b", "384", "-m", "PEM", "-N", ""],
                None,
            ),
            (
                "ec_pkcs8",
                &["-t", "ecdsa", "-b", "256", "-m", "PKCS8", "-N", ""],
                None,
            ),
            (
                "ed_openssh_pw",
                &["-t", "ed25519", "-N", "pw ed"],
                Some("pw ed"),
            ),
        ];
        for (name, args, pass) in cases {
            let path = ssh_keygen(dir.path(), name, args)?;
            let secret = pass.map(|p| Secret::new(p.to_owned()));
            let fp =
                key_file_fingerprint(&path, secret.as_ref()).map_err(|e| format!("{name}: {e}"))?;
            assert_eq!(fp, pub_fingerprint(&path)?, "{name}");
            if pass.is_some() {
                let wrong = Secret::new("nope".to_owned());
                match load_private_key(&path, Some(&wrong)) {
                    Err(TunnelError::KeyFile {
                        problem: KeyFileProblem::WrongPassphrase,
                        ..
                    }) => {}
                    other => panic!("{name}: expected WrongPassphrase, got {other:?}"),
                }
                match load_private_key(&path, None) {
                    Err(TunnelError::KeyFile {
                        problem: KeyFileProblem::PassphraseRequired,
                        ..
                    }) => {}
                    other => panic!("{name}: expected PassphraseRequired, got {other:?}"),
                }
            }
        }
        Ok(())
    }

    #[test]
    fn fingerprints_match_openssh() -> TestResult {
        // Public key and fingerprint as printed by `ssh-keygen -lf`.
        let line =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ test";
        let fp = fingerprint_openssh(line)?;
        assert!(fp.starts_with("SHA256:"), "{fp}");
        assert!(!fp.ends_with('='), "OpenSSH strips base64 padding: {fp}");
        // Same key, independent of the comment.
        let other = fingerprint_openssh(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ",
        )?;
        assert_eq!(fp, other);
        assert!(matches!(
            fingerprint_openssh("ssh-ed25519 not-base64"),
            Err(TunnelError::InvalidPublicKey)
        ));
        Ok(())
    }

    #[test]
    fn fingerprint_matches_ssh_keygen() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = ssh_keygen(dir.path(), "ed", &["-t", "ed25519", "-N", ""])?;
        let out = std::process::Command::new("ssh-keygen")
            .args(["-l", "-E", "sha256", "-f"])
            .arg(path.with_extension("pub"))
            .output()?;
        let printed = String::from_utf8(out.stdout)?;
        let expected = printed
            .split_whitespace()
            .nth(1)
            .ok_or("unexpected ssh-keygen output")?;
        assert_eq!(pub_fingerprint(&path)?, expected);
        Ok(())
    }
}
