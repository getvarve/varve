//! Loading TLS client private keys, including passphrase-protected ones
//! (design doc 3.1: the TLS key passphrase is a keychain secret).
//!
//! Drivers hand the result to their TLS library, so every driver accepts the
//! same key files, PEM or DER: unencrypted PKCS#8 (`PRIVATE KEY`), PKCS#1
//! (`RSA PRIVATE KEY`) and SEC1 (`EC PRIVATE KEY`); encrypted PKCS#8
//! (`ENCRYPTED PRIVATE KEY`, PBES2 with PBKDF2 or scrypt and AES or 3DES,
//! what OpenSSL 1.1 and later write); and legacy OpenSSL PEM encryption
//! (`Proc-Type: 4,ENCRYPTED` with AES-128/192/256-CBC or DES-EDE3-CBC).
//! Single DES and PBES1 (`openssl pkcs8 -v1`) are refused with a hint to
//! re-encrypt the key.
//!
//! Errors carry stable codes the app uses to prompt for the passphrase:
//! [`PASSPHRASE_REQUIRED`] and [`PASSPHRASE_WRONG`]. No error ever contains
//! the passphrase or key material.

use std::path::Path;

use base64ct::{Base64, Encoding};
use cbc::cipher::{BlockModeDecrypt, KeyIvInit, block_padding::Pkcs7};
use md5::Md5;
use pkcs8::pkcs5::{self, EncryptionScheme, pbes2};
use sha2::Digest;
use varve_secrets::Secret;
use zeroize::{Zeroize, Zeroizing};

use crate::{DriverError, ErrorKind, TlsMode, TlsParams};

/// Error code: the key file is encrypted and no passphrase was given.
pub const PASSPHRASE_REQUIRED: &str = "tls_key_passphrase_required";
/// Error code: the passphrase does not decrypt the key file.
pub const PASSPHRASE_WRONG: &str = "tls_key_passphrase_wrong";

/// The most an scrypt-encrypted key may ask for, as `128 * r * N * p`
/// bytes. It bounds memory (the scrypt crate allocates `128 * r * N` plus
/// `128 * r * p` bytes) and work (`N * r * p` ROMix rounds) together.
/// OpenSSL writes N = 2^14, r = 8, p = 1 (16 MiB, 2^17 rounds); the cap is
/// 16 times that, so a crafted key file cannot exhaust memory or hang the
/// connect for minutes.
const MAX_SCRYPT_COST: u64 = 256 * 1024 * 1024;

/// The most PBKDF2 iterations a key may ask for. OpenSSL writes 2048, and
/// current guidance for new keys is around 600 000 (PBKDF2-SHA256); ten
/// million is a few seconds of work.
const MAX_PBKDF2_ITERATIONS: u32 = 10_000_000;

/// The largest key file Varve reads. Key files are a few KB; anything
/// bigger is the wrong file.
const MAX_KEY_FILE_BYTES: u64 = 1024 * 1024;

/// How many decrypted keys [`load_client_identity`] keeps at most (see
/// [`forget_cached_keys`]).
const MAX_CACHED_KEYS: usize = 16;

/// How to re-encrypt a key in a supported format, for error messages.
const REENCRYPT_HINT: &str = "re-encrypt it, for example with `openssl pkcs8 -topk8 -v2 aes-256-cbc -in key.pem -out key-aes.pem`";

/// A decrypted private key in DER form, wiped from memory on drop.
#[derive(Clone, PartialEq, Eq)]
pub enum PrivateKeyDer {
    /// PKCS#8 `PrivateKeyInfo`.
    Pkcs8(Zeroizing<Vec<u8>>),
    /// PKCS#1 `RSAPrivateKey`.
    Pkcs1(Zeroizing<Vec<u8>>),
    /// SEC1 `ECPrivateKey`.
    Sec1(Zeroizing<Vec<u8>>),
}

impl PrivateKeyDer {
    /// The DER bytes.
    #[must_use]
    pub fn der(&self) -> &[u8] {
        match self {
            Self::Pkcs8(d) | Self::Pkcs1(d) | Self::Sec1(d) => d,
        }
    }

    /// The key as an unencrypted PEM document, for libraries that only take
    /// PEM. Wiped from memory on drop.
    #[must_use]
    pub fn to_pem(&self) -> Zeroizing<String> {
        let label = match self {
            Self::Pkcs8(_) => "PRIVATE KEY",
            Self::Pkcs1(_) => "RSA PRIVATE KEY",
            Self::Sec1(_) => "EC PRIVATE KEY",
        };
        let body = Zeroizing::new(Base64::encode_string(self.der()));
        let mut pem = Zeroizing::new(format!("-----BEGIN {label}-----\n"));
        for chunk in body.as_bytes().chunks(64) {
            pem.push_str(&String::from_utf8_lossy(chunk));
            pem.push('\n');
        }
        pem.push_str(&format!("-----END {label}-----\n"));
        pem
    }
}

impl std::fmt::Debug for PrivateKeyDer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Self::Pkcs8(_) => "Pkcs8",
            Self::Pkcs1(_) => "Pkcs1",
            Self::Sec1(_) => "Sec1",
        };
        write!(f, "PrivateKeyDer::{kind}([redacted])")
    }
}

/// The client certificate and key of `tls`, the key read and decrypted.
#[derive(Debug, Clone)]
pub struct ClientIdentity {
    /// PEM file with the client certificate chain.
    pub cert_path: String,
    /// The private key.
    pub key: PrivateKeyDer,
}

/// A key decrypted earlier: the same file contents and passphrase give the
/// same key, so the sessions of one connect (user, metadata, admin,
/// listener) derive it once.
struct CachedKey {
    path: std::path::PathBuf,
    digest: [u8; 32],
    passphrase: Secret,
    key: PrivateKeyDer,
    /// Times the key was reused.
    hits: u64,
}

static CACHE: std::sync::Mutex<Vec<CachedKey>> = std::sync::Mutex::new(Vec::new());

/// Bumped by every forget, under the cache lock. A derivation that started
/// before a forget does not store its key: it finished for a connect that
/// is already over.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn cache() -> std::sync::MutexGuard<'static, Vec<CachedKey>> {
    // Only a cache: a poisoned one is still consistent (entries are
    // inserted whole).
    CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn generation() -> u64 {
    GENERATION.load(std::sync::atomic::Ordering::SeqCst)
}

/// Drops the decrypted keys of `path` that [`load_client_identity`] keeps.
/// The app calls it when a profile using the file is edited or deleted,
/// its secrets change, its last session closes, or a connect or test
/// ends with no session of the profile open.
pub fn forget_cached_key(path: &str) {
    let mut cache = cache();
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    cache.retain(|entry| entry.path != Path::new(path));
}

/// Drops every decrypted key [`load_client_identity`] keeps.
pub fn forget_cached_keys() {
    let mut cache = cache();
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    cache.clear();
}

/// Whether a decrypted key of `path` is kept.
#[must_use]
pub fn is_key_cached(path: &str) -> bool {
    cache().iter().any(|entry| entry.path == Path::new(path))
}

/// Loads the client certificate path and private key of `tls`, decrypting
/// the key with [`TlsParams::client_key_passphrase`] when it is encrypted.
/// `None` when no client certificate is configured or TLS is disabled.
///
/// Key derivation (scrypt, PBKDF2) is slow on purpose, so the work runs on
/// tokio's blocking pool, and the call gives up after `timeout` (the
/// connect timeout). A key decrypted with a passphrase is kept, keyed by
/// file path, file contents and passphrase, until [`forget_cached_key`]:
/// the other sessions of the same connect reuse it instead of deriving it
/// again.
///
/// # Errors
///
/// [`ErrorKind::Tls`] when only one of certificate and key is set, or as
/// [`load_private_key`] (with its passphrase codes); [`ErrorKind::Timeout`]
/// when decrypting takes longer than `timeout`.
pub async fn load_client_identity(
    tls: &TlsParams,
    timeout: std::time::Duration,
) -> Result<Option<ClientIdentity>, DriverError> {
    if tls.mode == TlsMode::Disable {
        return Ok(None);
    }
    let (cert_path, key_path) = match (&tls.client_cert_path, &tls.client_key_path) {
        (None, None) => return Ok(None),
        (Some(cert), Some(key)) => (cert.clone(), key.clone()),
        (Some(_), None) => {
            return Err(tls_error(
                "a client certificate is set without its private key",
            ));
        }
        (None, Some(_)) => {
            return Err(tls_error(
                "a client private key is set without its certificate",
            ));
        }
    };
    let passphrase = tls.client_key_passphrase.clone();
    // The blocking derivation cannot be stopped: when the caller gives up,
    // the flag keeps its late result out of the cache.
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&cancelled);
    let work =
        tokio::task::spawn_blocking(move || load_cached(Path::new(&key_path), passphrase, &flag));
    let key = tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| {
            cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
            DriverError::new(
                ErrorKind::Timeout,
                "decrypting the client key took longer than the connect timeout",
            )
        })?
        .map_err(|e| tls_error(format!("loading the client key failed: {e}")))??;
    Ok(Some(ClientIdentity { cert_path, key }))
}

/// [`load_private_key`] through the cache of decrypted keys. The key is
/// stored only when the caller still waits (`cancelled` unset) and nothing
/// was forgotten since the derivation started.
fn load_cached(
    path: &Path,
    passphrase: Option<Secret>,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<PrivateKeyDer, DriverError> {
    let started = generation();
    let bytes = read_key_file(path)?;
    let Some(passphrase) = passphrase else {
        return parse_key_bytes(&bytes, None);
    };
    let digest: [u8; 32] = sha2::Sha256::digest(bytes.as_slice()).into();
    let hit = cache()
        .iter_mut()
        .find(|e| e.path == path && e.digest == digest && e.passphrase == passphrase)
        .map(|e| {
            e.hits += 1;
            e.key.clone()
        });
    if let Some(key) = hit {
        return Ok(key);
    }
    let key = parse_key_bytes(&bytes, Some(&passphrase));
    #[cfg(test)]
    finished().push(path.to_path_buf());
    let key = key?;
    let mut cache = cache();
    if cancelled.load(std::sync::atomic::Ordering::SeqCst) || generation() != started {
        return Ok(key);
    }
    if cache.len() >= MAX_CACHED_KEYS {
        cache.remove(0);
    }
    cache.push(CachedKey {
        path: path.to_path_buf(),
        digest,
        passphrase,
        key: key.clone(),
        hits: 0,
    });
    Ok(key)
}

/// Paths whose derivation finished, for tests that wait for a blocking
/// derivation the caller gave up on.
#[cfg(test)]
fn finished() -> std::sync::MutexGuard<'static, Vec<std::path::PathBuf>> {
    static FINISHED: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());
    FINISHED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Reads a key file: a regular file of at most [`MAX_KEY_FILE_BYTES`].
fn read_key_file(path: &Path) -> Result<Zeroizing<Vec<u8>>, DriverError> {
    use std::io::Read;
    let cannot = |e: std::io::Error| {
        tls_error(format!(
            "cannot read the client key file {}: {e}",
            path.display()
        ))
    };
    let meta = std::fs::metadata(path).map_err(cannot)?;
    if !meta.is_file() {
        return Err(tls_error(format!(
            "the client key {} is not a regular file",
            path.display()
        )));
    }
    if meta.len() > MAX_KEY_FILE_BYTES {
        return Err(too_big(path));
    }
    let file = std::fs::File::open(path).map_err(cannot)?;
    let mut bytes = Zeroizing::new(Vec::new());
    // The file may have grown since the size check.
    file.take(MAX_KEY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(cannot)?;
    if bytes.len() as u64 > MAX_KEY_FILE_BYTES {
        return Err(too_big(path));
    }
    Ok(bytes)
}

fn too_big(path: &Path) -> DriverError {
    tls_error(format!(
        "the client key file {} is larger than {} KiB; choose the key file itself",
        path.display(),
        MAX_KEY_FILE_BYTES / 1024
    ))
}

/// Parses PEM or DER key bytes.
fn parse_key_bytes(
    bytes: &[u8],
    passphrase: Option<&Secret>,
) -> Result<PrivateKeyDer, DriverError> {
    match std::str::from_utf8(bytes) {
        Ok(text) if text.contains("-----BEGIN") => parse_private_key(text, passphrase),
        _ => parse_private_key_der(bytes, passphrase),
    }
}

/// Reads a client key file (PEM or DER, a regular file of at most 1 MiB)
/// and decrypts it when needed.
///
/// # Errors
///
/// [`ErrorKind::Tls`] when the file cannot be read, is not a regular file,
/// is too big, holds no private key, is corrupt or uses an unsupported or
/// too costly encryption, with code [`PASSPHRASE_REQUIRED`] when it is
/// encrypted and `passphrase` is `None`, and [`PASSPHRASE_WRONG`] when the
/// passphrase does not decrypt it.
pub fn load_private_key(
    path: &Path,
    passphrase: Option<&Secret>,
) -> Result<PrivateKeyDer, DriverError> {
    parse_key_bytes(&read_key_file(path)?, passphrase)
}

/// Like [`load_private_key`], for PEM text already in memory.
///
/// # Errors
///
/// As [`load_private_key`].
pub fn parse_private_key(
    pem: &str,
    passphrase: Option<&Secret>,
) -> Result<PrivateKeyDer, DriverError> {
    let block = first_key_block(pem)?;
    match block.label.as_str() {
        "PRIVATE KEY" => Ok(PrivateKeyDer::Pkcs8(block.der)),
        "ENCRYPTED PRIVATE KEY" => decrypt_pkcs8(&block.der, passphrase),
        "RSA PRIVATE KEY" | "EC PRIVATE KEY" => {
            // RSAPrivateKey: INTEGER version, INTEGER modulus, ...;
            // ECPrivateKey: INTEGER 1, OCTET STRING privateKey, ...
            let second = if block.label == "RSA PRIVATE KEY" {
                0x02
            } else {
                0x04
            };
            let der = match block.encryption {
                None => {
                    if !is_key_structure(&block.der, second) {
                        return Err(tls_error(format!(
                            "the client key file holds a malformed {} block",
                            block.label
                        )));
                    }
                    block.der
                }
                Some(encryption) => {
                    let passphrase = passphrase.ok_or_else(passphrase_required)?;
                    let der = decrypt_legacy(&encryption, passphrase, &block.der)?;
                    // A wrong passphrase can still unpad correctly by
                    // chance; only a whole key structure counts.
                    if !is_key_structure(&der, second) {
                        return Err(passphrase_wrong());
                    }
                    der
                }
            };
            Ok(if block.label == "RSA PRIVATE KEY" {
                PrivateKeyDer::Pkcs1(der)
            } else {
                PrivateKeyDer::Sec1(der)
            })
        }
        other => Err(tls_error(format!(
            "the client key file holds a {other} block, not a private key"
        ))),
    }
}

/// Like [`load_private_key`], for a DER encoded key: encrypted or plain
/// PKCS#8, PKCS#1 or SEC1.
///
/// # Errors
///
/// As [`load_private_key`].
pub fn parse_private_key_der(
    der: &[u8],
    passphrase: Option<&Secret>,
) -> Result<PrivateKeyDer, DriverError> {
    if pkcs8::PrivateKeyInfoRef::try_from(der).is_ok() {
        return Ok(PrivateKeyDer::Pkcs8(Zeroizing::new(der.to_vec())));
    }
    if pkcs8::EncryptedPrivateKeyInfoRef::try_from(der).is_ok() {
        return decrypt_pkcs8(der, passphrase);
    }
    // RSAPrivateKey starts with INTEGER version, INTEGER modulus;
    // ECPrivateKey with INTEGER 1, OCTET STRING privateKey.
    match second_element_tag(der) {
        Some(0x02) => Ok(PrivateKeyDer::Pkcs1(Zeroizing::new(der.to_vec()))),
        Some(0x04) => Ok(PrivateKeyDer::Sec1(Zeroizing::new(der.to_vec()))),
        _ => Err(tls_error(
            "the client key file holds no PEM or DER private key",
        )),
    }
}

/// True when `der` is exactly one DER `SEQUENCE` (no bytes after it) that
/// starts with an `INTEGER` followed by an element tagged `second`.
fn is_key_structure(der: &[u8], second: u8) -> bool {
    der_header_and_len(der) == Some(der.len()) && second_element_tag(der) == Some(second)
}

/// The tag of the second element of a DER `SEQUENCE` whose first element
/// is an `INTEGER`.
fn second_element_tag(der: &[u8]) -> Option<u8> {
    let (tag, body) = der_element(der)?;
    if tag != 0x30 {
        return None;
    }
    let (first, _) = der_element(body)?;
    if first != 0x02 {
        return None;
    }
    let first_len = der_header_and_len(body)?;
    body.get(first_len).copied()
}

/// Tag and content of the DER element at the start of `data`.
fn der_element(data: &[u8]) -> Option<(u8, &[u8])> {
    let tag = *data.first()?;
    let (header, len) = der_length(data)?;
    Some((tag, data.get(header..header.checked_add(len)?)?))
}

/// Total length (header plus content) of the DER element at the start of
/// `data`.
fn der_header_and_len(data: &[u8]) -> Option<usize> {
    let (header, len) = der_length(data)?;
    header.checked_add(len)
}

/// Header length and content length of the DER element at the start of
/// `data` (definite lengths only, as DER requires).
fn der_length(data: &[u8]) -> Option<(usize, usize)> {
    let first = *data.get(1)?;
    if first < 0x80 {
        return Some((2, usize::from(first)));
    }
    let count = usize::from(first & 0x7f);
    if count == 0 || count > 4 {
        return None;
    }
    let mut len = 0usize;
    for byte in data.get(2..2 + count)? {
        len = (len << 8) | usize::from(*byte);
    }
    Some((2 + count, len))
}

/// Decrypts an `EncryptedPrivateKeyInfo` (PKCS#8, PBES2).
fn decrypt_pkcs8(der: &[u8], passphrase: Option<&Secret>) -> Result<PrivateKeyDer, DriverError> {
    let info = pkcs8::EncryptedPrivateKeyInfoRef::try_from(der).map_err(|_| {
        tls_error(format!(
            "the client key is encrypted in a way Varve cannot read (only PBES2 with PBKDF2 or \
             scrypt and AES or 3DES is supported); {REENCRYPT_HINT}"
        ))
    })?;
    match &info.encryption_algorithm {
        EncryptionScheme::Pbes1(_) => {
            return Err(tls_error(format!(
                "the client key uses the outdated PBES1 encryption; {REENCRYPT_HINT}"
            )));
        }
        EncryptionScheme::Pbes2(parameters) => {
            check_kdf_cost(&parameters.kdf)?;
            check_block_aligned(&parameters.encryption, info.encrypted_data.as_bytes().len())?;
        }
        _ => {
            return Err(tls_error(format!(
                "the client key uses an encryption scheme Varve does not support; \
                 {REENCRYPT_HINT}"
            )));
        }
    }
    let passphrase = passphrase.ok_or_else(passphrase_required)?;
    let document = info
        .decrypt(passphrase.expose().as_bytes())
        .map_err(|error| match error {
            pkcs8::Error::EncryptedPrivateKey(pkcs5::Error::DecryptFailed) => passphrase_wrong(),
            pkcs8::Error::EncryptedPrivateKey(
                pkcs5::Error::UnsupportedAlgorithm { .. } | pkcs5::Error::NoPbes1CryptSupport,
            ) => tls_error(format!(
                "the client key uses an encryption algorithm Varve does not support; \
                 {REENCRYPT_HINT}"
            )),
            pkcs8::Error::EncryptedPrivateKey(_) => {
                tls_error("the encryption parameters of the client key are invalid")
            }
            // Decrypted, but the result is no PrivateKeyInfo: a wrong
            // passphrase that happened to unpad correctly.
            _ => passphrase_wrong(),
        })?;
    let der = Zeroizing::new(document.as_bytes().to_vec());
    // A wrong passphrase can still unpad correctly by chance; the result
    // must parse as a PrivateKeyInfo.
    pkcs8::PrivateKeyInfoRef::try_from(der.as_slice()).map_err(|_| passphrase_wrong())?;
    Ok(PrivateKeyDer::Pkcs8(der))
}

/// Refuses key derivation parameters above [`MAX_SCRYPT_COST`] or
/// [`MAX_PBKDF2_ITERATIONS`].
fn check_kdf_cost(kdf: &pbes2::Kdf) -> Result<(), DriverError> {
    if let Some(scrypt) = kdf.scrypt() {
        let cost = 128u64
            .saturating_mul(u64::from(scrypt.block_size))
            .saturating_mul(scrypt.cost_parameter)
            .saturating_mul(u64::from(scrypt.parallelization).max(1));
        if cost > MAX_SCRYPT_COST {
            return Err(too_costly(format!(
                "scrypt parameters (N = {}, r = {}, p = {})",
                scrypt.cost_parameter, scrypt.block_size, scrypt.parallelization
            )));
        }
    }
    if let Some(pbkdf2) = kdf.pbkdf2()
        && pbkdf2.iteration_count > MAX_PBKDF2_ITERATIONS
    {
        return Err(too_costly(format!(
            "{} PBKDF2 iterations",
            pbkdf2.iteration_count
        )));
    }
    Ok(())
}

fn too_costly(what: String) -> DriverError {
    tls_error(format!(
        "the client key asks for {what}, more work than Varve allows (scrypt up to 16 times and \
         PBKDF2 up to about 4800 times what OpenSSL writes); {REENCRYPT_HINT}"
    ))
}

/// A CBC ciphertext that is not a whole number of blocks is a corrupt
/// file, not a wrong passphrase.
fn check_block_aligned(scheme: &pbes2::EncryptionScheme, len: usize) -> Result<(), DriverError> {
    let block = match scheme {
        pbes2::EncryptionScheme::Aes128Cbc { .. }
        | pbes2::EncryptionScheme::Aes192Cbc { .. }
        | pbes2::EncryptionScheme::Aes256Cbc { .. } => 16,
        pbes2::EncryptionScheme::DesEde3Cbc { .. } => 8,
        _ => return Ok(()),
    };
    if len == 0 || !len.is_multiple_of(block) {
        return Err(corrupt());
    }
    Ok(())
}

fn corrupt() -> DriverError {
    tls_error(
        "the encrypted client key is corrupt (truncated or damaged); no passphrase can open it",
    )
}

/// Legacy OpenSSL PEM encryption (`DEK-Info`).
struct LegacyEncryption {
    cipher: String,
    iv: Vec<u8>,
}

struct KeyBlock {
    label: String,
    encryption: Option<LegacyEncryption>,
    der: Zeroizing<Vec<u8>>,
}

fn first_key_block(pem: &str) -> Result<KeyBlock, DriverError> {
    let mut lines = pem.lines().map(str::trim);
    while let Some(line) = lines.next() {
        let Some(label) = line
            .strip_prefix("-----BEGIN ")
            .and_then(|rest| rest.strip_suffix("-----"))
        else {
            continue;
        };
        if !label.contains("PRIVATE KEY") {
            continue;
        }
        let end = format!("-----END {label}-----");
        let mut headers = Vec::new();
        let mut body = Zeroizing::new(String::new());
        let mut closed = false;
        for line in lines.by_ref() {
            if line == end {
                closed = true;
                break;
            }
            if body.is_empty() && line.contains(':') {
                headers.push(line.to_string());
            } else {
                body.push_str(line);
            }
        }
        if !closed {
            return Err(tls_error(format!("the {label} block has no END line")));
        }
        let der = Zeroizing::new(
            Base64::decode_vec(&body)
                .map_err(|_| tls_error(format!("the {label} block is not valid base64")))?,
        );
        let encryption = legacy_encryption(&headers)?;
        return Ok(KeyBlock {
            label: label.to_string(),
            encryption,
            der,
        });
    }
    Err(tls_error("the client key file holds no private key"))
}

fn legacy_encryption(headers: &[String]) -> Result<Option<LegacyEncryption>, DriverError> {
    let encrypted = headers.iter().any(|h| {
        h.split_once(':')
            .is_some_and(|(k, v)| k.trim() == "Proc-Type" && v.trim() == "4,ENCRYPTED")
    });
    if !encrypted {
        return Ok(None);
    }
    let info = headers
        .iter()
        .find_map(|h| {
            h.split_once(':')
                .filter(|(k, _)| k.trim() == "DEK-Info")
                .map(|(_, v)| v.trim().to_string())
        })
        .ok_or_else(|| tls_error("the encrypted key has no DEK-Info header"))?;
    let (cipher, iv_hex) = info
        .split_once(',')
        .ok_or_else(|| tls_error("the DEK-Info header is malformed"))?;
    let iv = decode_hex(iv_hex.trim())
        .ok_or_else(|| tls_error("the DEK-Info initialisation vector is not hex"))?;
    Ok(Some(LegacyEncryption {
        cipher: cipher.trim().to_ascii_uppercase(),
        iv,
    }))
}

fn decrypt_legacy(
    encryption: &LegacyEncryption,
    passphrase: &Secret,
    data: &[u8],
) -> Result<Zeroizing<Vec<u8>>, DriverError> {
    let (key_len, iv_len) = match encryption.cipher.as_str() {
        "AES-128-CBC" => (16, 16),
        "AES-192-CBC" => (24, 16),
        "AES-256-CBC" => (32, 16),
        "DES-EDE3-CBC" => (24, 8),
        "DES-CBC" => {
            return Err(tls_error(
                "the client key is encrypted with single DES, which is insecure; re-encrypt it, for example with `openssl pkey -in key.pem -aes256 -out key-aes.pem`",
            ));
        }
        other => {
            return Err(tls_error(format!(
                "the client key uses the unsupported cipher {other}"
            )));
        }
    };
    if encryption.iv.len() != iv_len {
        return Err(tls_error(
            "the DEK-Info initialisation vector has the wrong length",
        ));
    }
    if data.is_empty() || !data.len().is_multiple_of(iv_len) {
        return Err(corrupt());
    }
    let key = evp_bytes_to_key(passphrase.expose().as_bytes(), &encryption.iv[..8], key_len);
    let iv = encryption.iv.as_slice();
    let plain = match encryption.cipher.as_str() {
        "AES-128-CBC" => cbc::Decryptor::<aes::Aes128>::new_from_slices(&key, iv)
            .map_err(|_| passphrase_wrong())?
            .decrypt_padded_vec::<Pkcs7>(data),
        "AES-192-CBC" => cbc::Decryptor::<aes::Aes192>::new_from_slices(&key, iv)
            .map_err(|_| passphrase_wrong())?
            .decrypt_padded_vec::<Pkcs7>(data),
        "AES-256-CBC" => cbc::Decryptor::<aes::Aes256>::new_from_slices(&key, iv)
            .map_err(|_| passphrase_wrong())?
            .decrypt_padded_vec::<Pkcs7>(data),
        _ => cbc::Decryptor::<des::TdesEde3>::new_from_slices(&key, iv)
            .map_err(|_| passphrase_wrong())?
            .decrypt_padded_vec::<Pkcs7>(data),
    };
    plain.map(Zeroizing::new).map_err(|_| passphrase_wrong())
}

/// OpenSSL `EVP_BytesToKey` with MD5 and one iteration, as used by legacy
/// PEM encryption.
fn evp_bytes_to_key(passphrase: &[u8], salt: &[u8], key_len: usize) -> Zeroizing<Vec<u8>> {
    let mut key = Zeroizing::new(Vec::with_capacity(key_len + 16));
    let mut previous: Option<Vec<u8>> = None;
    while key.len() < key_len {
        let mut hasher = Md5::new();
        if let Some(previous) = &previous {
            hasher.update(previous);
        }
        hasher.update(passphrase);
        hasher.update(salt);
        let mut digest = hasher.finalize().to_vec();
        key.extend_from_slice(&digest);
        if let Some(mut old) = previous.replace(digest.clone()) {
            old.zeroize();
        }
        digest.zeroize();
    }
    if let Some(mut last) = previous {
        last.zeroize();
    }
    key.truncate(key_len);
    key
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

fn tls_error(message: impl Into<String>) -> DriverError {
    DriverError::new(ErrorKind::Tls, message)
}

fn passphrase_required() -> DriverError {
    tls_error("the client key file is encrypted; enter its passphrase")
        .with_code(PASSPHRASE_REQUIRED)
}

fn passphrase_wrong() -> DriverError {
    tls_error("the passphrase does not decrypt the client key file").with_code(PASSPHRASE_WRONG)
}

#[cfg(test)]
mod tests {
    //! The keys below are throwaway test vectors generated with OpenSSL 4
    //! for these tests only (passphrase `test-pass`). They protect nothing
    //! and are not used anywhere else.

    use super::*;

    const PKCS8_PLAIN: &str = r"-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg7pKInxgChYINiDXn
KDPUegtWIsSeMVQ1iz6c/UK+36uhRANCAASsr2l13dvJh2tuTIbebxQcRK06N9Ne
SkZ6zxErizQVNxQpA9dOKBrBjhTRVY/Mf41dQH7Wujau6KhCKwFcDdv+
-----END PRIVATE KEY-----
";

    const PKCS8_AES: &str = r"-----BEGIN ENCRYPTED PRIVATE KEY-----
MIH0MF8GCSqGSIb3DQEFDTBSMDEGCSqGSIb3DQEFDDAkBBDdgtugUMOTqN/JVgCp
hfjMAgIIADAMBggqhkiG9w0CCQUAMB0GCWCGSAFlAwQBKgQQBzpD3BRFvPg9HLyr
Ju+GeQSBkHrpVI2umb/fYNAvEDuRHkCg21rjBmvB6XdXJ6ZWSuplk6IUvYlM9Cp5
DwSPX2iVtGMoPBVIJ9CaoIRLHEsxnhqrTEK3Req79cgeQbDKw1Qo6+u1MUGxyjSb
SIbbLhjf1aRwDvu5AxbdrHRTzFXFgwNtBOaMOxvGozRbcHe0LwETWyPEQKSa6lQT
4JMsc/9b8A==
-----END ENCRYPTED PRIVATE KEY-----
";

    const PKCS8_3DES: &str = r"-----BEGIN ENCRYPTED PRIVATE KEY-----
MIHrMFYGCSqGSIb3DQEFDTBJMDEGCSqGSIb3DQEFDDAkBBC0hKxydMaL3oSgBrQ0
/KH9AgIIADAMBggqhkiG9w0CCQUAMBQGCCqGSIb3DQMHBAhSQvwoAIy6BQSBkJqY
8J2/o/5UnECwyTTDEP8lbcO8DK7KP0ij3M4kMQ5RMqMR+2oCd1Kp7PemvMishRaH
Hy+RUJxmIPL9J90J88vwi1hh825zrCimVYO9vNQEWszSv1m2b0f+S/BIaignhfNm
fl/723NnIiGvP2+NbxpAuyMqQwpX/AEJQwc2ZERMDZNdz2tdiwijrD0kMX/grQ==
-----END ENCRYPTED PRIVATE KEY-----
";

    const SEC1_PLAIN: &str = r"-----BEGIN EC PRIVATE KEY-----
MHcCAQEEIO6SiJ8YAoWCDYg15ygz1HoLViLEnjFUNYs+nP1Cvt+roAoGCCqGSM49
AwEHoUQDQgAErK9pdd3byYdrbkyG3m8UHEStOjfTXkpGes8RK4s0FTcUKQPXTiga
wY4U0VWPzH+NXUB+1ro2ruioQisBXA3b/g==
-----END EC PRIVATE KEY-----
";

    const SEC1_AES128: &str = r"-----BEGIN EC PRIVATE KEY-----
Proc-Type: 4,ENCRYPTED
DEK-Info: AES-128-CBC,D26397B43AEA2041809D394EE2D6B4F5

rA8w5rKqGtGOeMqOsdvpNBqyGa9wH2fXeNcaqlP5pGm+08624pWfRlvNHJcNpWwK
B4DrQDQM3Gwne5h8jouXhrNmLAD2g9WiIIe34xbv9s6EWD7DpBLSUeu4QmlBIZ1K
ELV7EA4rNRedOvO5JIh7KP4fTR8GgXS4/HVNzi/PcRI=
-----END EC PRIVATE KEY-----
";

    const RSA: &str = r"-----BEGIN RSA PRIVATE KEY-----
MIICXQIBAAKBgQDBAvuC+5gmGdpYWIl7jSZxluZWY2vT1mI6pr+UXJWv8yig1tNj
i/cUaLc+MFUnZjku6jFlunHkMUqi9IM+eIlCegPf3fer+p+wCpRAHuDkGPDsnXe6
0a53e+CnxDPml8j8y+JTKYltD/PYlW2/SMSqOj0m4W0iEpLeUrwMl2tPuwIDAQAB
AoGAZ3jv2RKESLjHY2FjowbSu0aIiXuXTOO7mXHXdvQDk2YqO7+vZ8ibVyekRQSo
Qgz9TvdiMAU6FXmPZ7jpu03nVvnius23fk3vjOXYB9xeEJ7V2CpwzUuikc/ztkI9
iTVC+u+OGL/zXc7PC2r+GGiKP34V4nP4OerqQDFlvbroGkECQQD7o8Crx9/gtJn4
s7TFg1gTh78n97WiMiZIKWe1Fp7xxjlsQ3IXTgXk89QPPkVOLMFtpS0WANlcC9W2
Hw4vpisrAkEAxFspfukEYq5mKo6AchxMJF/m6Pt32wIZbZ3vjzIuZxHQv1d4aL/x
tcj1xreep2g91++RnWlp/SkW5UnGZb7lsQJATS9PO+GU9BGxYK2Odz7wVx0+Lv42
AkgrodrBarRDkJlip6iQaEGQYUfA9eP90hq8Qet2Rtd/1FH0/WXNLiRj/QJBAKYC
8T10YxnPm19lpeFUXWfvkTnvyjDnQHp2KmFmKudHn7b2jD378q8gHfQJsOCW+JqF
2RY+VrcMs2feDRhe4rECQQCU2KAhwsP5uEnJpb7HbwSc3DGBA3l24izg1S+mZt1h
2FqTqbGbK9FQ7Qj7HplR47wGJCWK+8CPbCD+P+TxLZZr
-----END RSA PRIVATE KEY-----
";

    const RSA_3DES: &str = r"-----BEGIN RSA PRIVATE KEY-----
Proc-Type: 4,ENCRYPTED
DEK-Info: DES-EDE3-CBC,E85684B48A2FEBB3

uzb0ClCDr3dCDxD13XqzF7TpeUxzWj+5y585rAGdjq7vuGCxiTwcGTSjaKuZHWDP
RyByJwboVcEOE0E/r+n9oWKHC9lyf9buA7R8W0ciVk+OT2nOpALF9VW10h1YA6Zz
ZuVNdxcNg2D9aAR9ItsGQuV0XlJ2uG1pJqkugOmzf3CUUwtO6jOQBf399vl8/i+o
q2KyLeKWeVH8EdP0LXvCs4U93Fs21ONcuL7eqedahrW+nXp58dU0xdk6nNj3g9Ib
XdZzaIqh9G0I1Kk5i0oTkfvPTcA/IAvoZJTJTEa/8+FE03GLZLLDsBTsrMSNAVEJ
vU+y3qVFSvKtz5ICHk7ezRt4vZjDrBlN4SiNthCo7eu8L/32UTH9QPeiiEvgmPVo
KQt2L+Ulw9iGORuqrrcfAQA8fG+dGUjUK6lO1fLyRicsZQaKJY+QSPK0l1joRbF3
48dmiMmVXIObo886t1FyPN1tg2dvYWB5+diFEcX63mZVXDSbNC05VRTyxFBbCHtV
FY12ehrBidgcZeL32UuSXp+ZT3Ccchp0vgjl0DFANGpmRRb/QtW9o/XweeHoPzEm
KXZ2SxjGcXmJX9beevRmQYbwxgcutRcfg/tSia5NnoGfzzvVX+xsQuXT9InNjimO
Ssx502tD2I27LQAMIqFVx7ce16jbHLlOMCDevE8V2E0hUl5iCxlC0qL15VvF/AW0
+aQRqBLXhalX1Oq27vqIvztX0+lHCmepcZiD6YszWyeJIAjOWtd/6lVBVCyHmTa0
QUu/5h3Hm2zGVX1VFvSfsZGCDmLwE9OALXhiT5iUAW3v8r0hX/OjVQ==
-----END RSA PRIVATE KEY-----
";

    const RSA_AES256: &str = r"-----BEGIN RSA PRIVATE KEY-----
Proc-Type: 4,ENCRYPTED
DEK-Info: AES-256-CBC,55826DD19AEF2C996E214A7DEA47CA31

RQc5fVrETRdteWmF76LYZVmqIW8riqverk+StP/iRr2d4r65kNkA2SBtTVFgbk80
3pOywAmU6kLqtW8QMziVOCgsGROWzN+gK1Kpz2BfZJ0lPoTyJohBPsEXs9oRdtD5
wqfCvUtzLXcQMeIZ2cRTLdUvF1PVLXH9keiEFXDTr9GxoDpGSCdHv5xWs5r1683x
kFELZODtxmgnTrB8mNpEs3kd5JPnyCTc2tg+EKRT/UuKDev+vaJSdnBmpVge0rj5
T7HEKQKF9qX5iFDWGpuXP+8bQOcoah49kAPiznysQrGZasS5VHBgX3C1cCfKPzNH
RpI8Kgzrn/DX/ROnjd84HtIwCKE3pRyspCXXUTGKCpKr3sR5F505Wstg9wYdR9lF
eCDkKjJHO/RG6LOOvsjm2/j71RJPugiNXVrcWcEFv/LKo+Q9Nvuw/g654/xJMynq
LhLQ/EGO9M2+jTrnGNEhM46PN4ETviTtU1bSHnit3JgEJ8tTk+62PT+PdeRdA3Q/
w5GJRGPvxeOBRnYjn/emEakJyQq29WIg3IRRUk35Jpw+nAS43YZRmp+75wRxR8Jx
G2L8277r+c2VeKZVQoedujxuv1Brkq23xGO7hTE7R1oRt7qWWoaXDJ2JdVhUGe+K
8srBJKq7G1Xmuz1hLyZ4tGV3nNoAaSyxZeDg2fG/saIK0U4WORlofxfnpgVPwAQy
9FweBJIklxMi17TupLwR6i2IsT1Jc/Kqjw25BBU7Bw1Ye/AppO8YxhH+5OmGRNml
nnGPlZpeE4LohuU9OlVKKvj3td2+fIJU8wXl6cxZjKBQJF84ec6QRSUGByZwd+Nu
-----END RSA PRIVATE KEY-----
";

    /// `openssl pkcs8 -topk8 -scrypt` (scrypt N=16384, r=8, p=1, AES-256-CBC).
    const PKCS8_SCRYPT: &str = r"-----BEGIN ENCRYPTED PRIVATE KEY-----
MIHsMFcGCSqGSIb3DQEFDTBKMCkGCSsGAQQB2kcECzAcBBA7xzxOpgKLQcArKZQ2
PrZBAgJAAAIBCAIBATAdBglghkgBZQMEASoEEPln28D+ZwGiEH/GtmHSaj4EgZAS
bMwoIPXjcHZBH1YEO5Wy42Ppe1zK1h0oq4hRd4dNme5n2KAFXjoVNDc10akWl93R
awU/O70KF23x71w1ETLzZmbfsGWltQWH8MTui82Pbbqnpn6wXKvF4LKKCjwKHORA
SX1Qf/PxSDHl5a/6MGAtprcN6V6PhDibaqlB5VTLNnIQOOChyX/Ivi9PEwomhAQ=
-----END ENCRYPTED PRIVATE KEY-----
";

    /// `openssl pkcs8 -topk8 -v2 aes-128-cbc -v2prf hmacWithSHA1`, the PRF
    /// OpenSSL 1.0 used.
    const PKCS8_PBKDF2_SHA1: &str = r"-----BEGIN ENCRYPTED PRIVATE KEY-----
MIHmMFEGCSqGSIb3DQEFDTBEMCMGCSqGSIb3DQEFDDAWBBA0ROFo3HVtGNP+VEjR
2o3iAgIIADAdBglghkgBZQMEAQIEEKGiR44bKpCC7aXqiumdKLQEgZAACDvsNuzv
k9mydHpY0ohtuaYxsGnBgn3AcUML20M/yGvSEzQkp+UPuuqvVxbSdQMQ4g2G93Va
ywyh/3DHi6lgw11/LE3EZhnfRVRnCZ7ge2hiX0LqnPcEaFevxIUFkcIiWESfnbYQ
elkazw9Vzg/MiphKdus0rH+SMwqkiGbufT00gtBM8bQnHK2iAwIE0vM=
-----END ENCRYPTED PRIVATE KEY-----
";

    /// `openssl pkcs8 -topk8 -v1 PBE-SHA1-3DES`: PKCS#12 PBE, refused.
    const PKCS8_PBES1: &str = r"-----BEGIN ENCRYPTED PRIVATE KEY-----
MIGxMBwGCiqGSIb3DQEMAQMwDgQIDG5FS5Zl7r0CAggABIGQZ1lsUqum9P3GZf7K
kT5fpoZwOaM+sCzUIsZDYTL6Son9fQ6sPKRdltWICy15gDB/+oeAMP+iFzLhZaCk
M9hSAwn7gycD2a5kn3sG+EcA30IOyJyzsfNdaMBYNbC7wNod0xjQHP6cHQy4FRND
USUK83q9qsMab8grBWCCNzwjqZNBYp3d7Lmq/M0s0ZnmFDL1
-----END ENCRYPTED PRIVATE KEY-----
";

    fn pass(text: &str) -> Secret {
        Secret::new(text.to_string())
    }

    #[test]
    fn plain_keys_load() -> Result<(), DriverError> {
        assert!(matches!(
            parse_private_key(PKCS8_PLAIN, None)?,
            PrivateKeyDer::Pkcs8(_)
        ));
        assert!(matches!(
            parse_private_key(SEC1_PLAIN, None)?,
            PrivateKeyDer::Sec1(_)
        ));
        assert!(matches!(
            parse_private_key(RSA, None)?,
            PrivateKeyDer::Pkcs1(_)
        ));
        Ok(())
    }

    #[test]
    fn encrypted_pkcs8_decrypts_to_the_plain_key() -> Result<(), DriverError> {
        let plain = parse_private_key(PKCS8_PLAIN, None)?;
        for encrypted in [PKCS8_AES, PKCS8_3DES, PKCS8_SCRYPT, PKCS8_PBKDF2_SHA1] {
            assert_eq!(
                parse_private_key(encrypted, Some(&pass("test-pass")))?,
                plain
            );
        }
        Ok(())
    }

    #[test]
    fn a_passphrase_for_a_plain_key_is_ignored() -> Result<(), DriverError> {
        for plain in [PKCS8_PLAIN, SEC1_PLAIN, RSA] {
            assert_eq!(
                parse_private_key(plain, Some(&pass("unused")))?,
                parse_private_key(plain, None)?
            );
        }
        Ok(())
    }

    #[test]
    fn der_keys_load_like_pem() -> Result<(), Box<dyn std::error::Error>> {
        for pem in [PKCS8_PLAIN, SEC1_PLAIN, RSA] {
            let key = parse_private_key(pem, None)?;
            assert_eq!(parse_private_key_der(key.der(), None)?, key);
        }
        let encrypted = first_key_block(PKCS8_SCRYPT)?.der;
        assert_eq!(
            parse_private_key_der(&encrypted, Some(&pass("test-pass")))?,
            parse_private_key(PKCS8_PLAIN, None)?
        );
        assert_eq!(
            parse_private_key_der(&encrypted, None)
                .err()
                .and_then(|e| e.code)
                .as_deref(),
            Some(PASSPHRASE_REQUIRED)
        );
        assert!(parse_private_key_der(b"\x30\x03\x01\x01\x00", None).is_err());
        assert!(parse_private_key_der(b"", None).is_err());
        Ok(())
    }

    #[test]
    fn key_files_load_from_disk() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let pem = dir.path().join("key.pem");
        std::fs::write(&pem, PKCS8_SCRYPT)?;
        let der = dir.path().join("key.der");
        std::fs::write(&der, parse_private_key(RSA, None)?.der())?;
        assert_eq!(
            load_private_key(&pem, Some(&pass("test-pass")))?,
            parse_private_key(PKCS8_PLAIN, None)?
        );
        assert_eq!(load_private_key(&der, None)?, parse_private_key(RSA, None)?);
        let missing = load_private_key(&dir.path().join("nope.pem"), None).err();
        assert!(
            missing.is_some_and(|e| e.kind == ErrorKind::Tls && e.message.contains("nope.pem"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn client_identity_needs_both_files_and_uses_the_passphrase()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let key = dir.path().join("key.pem");
        std::fs::write(&key, PKCS8_SCRYPT)?;
        let key = key.display().to_string();
        let mut tls = TlsParams {
            mode: TlsMode::Require,
            client_cert_path: Some("/client.crt".into()),
            client_key_path: Some(key.clone()),
            ..TlsParams::default()
        };
        let required = load_client_identity(&tls, LONG).await.err();
        assert_eq!(
            required.and_then(|e| e.code).as_deref(),
            Some(PASSPHRASE_REQUIRED)
        );
        tls.client_key_passphrase = Some(pass("wrong"));
        let wrong = load_client_identity(&tls, LONG).await.err();
        assert_eq!(
            wrong.and_then(|e| e.code).as_deref(),
            Some(PASSPHRASE_WRONG)
        );
        tls.client_key_passphrase = Some(pass("test-pass"));
        let identity = load_client_identity(&tls, LONG)
            .await?
            .ok_or("no client identity")?;
        assert_eq!(identity.cert_path, "/client.crt");
        assert_eq!(identity.key, parse_private_key(PKCS8_PLAIN, None)?);

        // TLS off: nothing is read (not even the missing passphrase).
        tls.mode = TlsMode::Disable;
        tls.client_key_passphrase = None;
        assert!(load_client_identity(&tls, LONG).await?.is_none());

        tls.mode = TlsMode::Prefer;
        tls.client_cert_path = None;
        assert!(load_client_identity(&tls, LONG).await.is_err());
        tls.client_cert_path = Some("/client.crt".into());
        tls.client_key_path = None;
        assert!(load_client_identity(&tls, LONG).await.is_err());
        tls.client_cert_path = None;
        assert!(load_client_identity(&tls, LONG).await?.is_none());
        Ok(())
    }

    #[test]
    fn pbes1_and_huge_scrypt_costs_are_refused() -> Result<(), Box<dyn std::error::Error>> {
        let error = parse_private_key(PKCS8_PBES1, Some(&pass("test-pass")))
            .err()
            .ok_or("PBES1 was accepted")?;
        assert_eq!(error.code, None);
        assert!(error.message.contains("openssl pkcs8"), "{}", error.message);

        // The same scrypt key with N raised from 2^14 to 2^24 (16 GiB).
        let der = first_key_block(PKCS8_SCRYPT)?.der.to_vec();
        let at = der
            .windows(4)
            .position(|w| w == [0x02, 0x02, 0x40, 0x00])
            .ok_or("no scrypt cost in the vector")?;
        let huge = rebuild_with_cost(&der, at)?;
        let error = parse_private_key_der(&huge, Some(&pass("test-pass")))
            .err()
            .ok_or("a 16 GiB scrypt key was accepted")?;
        assert!(error.message.contains("N = 16777216"), "{}", error.message);
        Ok(())
    }

    /// Replaces the bytes `old` at `at` with `new` in the PKCS#8 vectors
    /// (both share one layout), growing every enclosing length. Length
    /// byte offsets, from `openssl asn1parse`: outer (2, long form 0x81),
    /// algorithm (4), PBES2 params (17), KDF (19), KDF params (32).
    fn replace_in_kdf(
        der: &[u8],
        old: &[u8],
        new: &[u8],
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let at = der
            .windows(old.len())
            .position(|w| w == old)
            .ok_or("pattern not in the vector")?;
        let grow = u8::try_from(new.len() - old.len())?;
        let mut out = der.to_vec();
        out.splice(at..at + old.len(), new.iter().copied());
        for index in [32usize, 19, 17, 4, 2] {
            let len = out.get_mut(index).ok_or("vector too short")?;
            *len += grow;
        }
        Ok(out)
    }

    /// Re-encodes the scrypt vector with cost 2^24.
    fn rebuild_with_cost(der: &[u8], _at: usize) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        replace_in_kdf(
            der,
            &[0x02, 0x02, 0x40, 0x00],
            &[0x02, 0x04, 0x01, 0x00, 0x00, 0x00],
        )
    }

    /// Long enough for any test key.
    const LONG: std::time::Duration = std::time::Duration::from_secs(120);

    #[test]
    fn scrypt_parallelism_and_pbkdf2_iterations_are_capped()
    -> Result<(), Box<dyn std::error::Error>> {
        // p raised from 1 to 64: 16 MiB per lane, 1 GiB of work in all.
        let scrypt = first_key_block(PKCS8_SCRYPT)?.der.to_vec();
        let at = scrypt
            .windows(6)
            .position(|w| w == [0x02, 0x01, 0x08, 0x02, 0x01, 0x01])
            .ok_or("no scrypt r and p in the vector")?;
        let mut parallel = scrypt.clone();
        if let Some(p) = parallel.get_mut(at + 5) {
            *p = 64;
        }
        let error = parse_private_key_der(&parallel, None)
            .err()
            .ok_or("p = 64 was accepted")?;
        assert_eq!(error.code, None, "{error:?}");
        assert!(error.message.contains("p = 64"), "{}", error.message);

        // 20 million PBKDF2 iterations instead of 2048.
        let pbkdf2 = first_key_block(PKCS8_AES)?.der.to_vec();
        let slow = replace_in_kdf(
            &pbkdf2,
            &[0x02, 0x02, 0x08, 0x00],
            &[0x02, 0x04, 0x01, 0x31, 0x2d, 0x00],
        )?;
        let error = parse_private_key_der(&slow, Some(&pass("test-pass")))
            .err()
            .ok_or("20 million iterations were accepted")?;
        assert_eq!(error.code, None, "{error:?}");
        assert!(
            error.message.contains("20000000 PBKDF2 iterations"),
            "{}",
            error.message
        );
        Ok(())
    }

    #[test]
    fn key_files_must_be_small_regular_files() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let error = load_private_key(dir.path(), None)
            .err()
            .ok_or("a directory was read")?;
        assert!(
            error.message.contains("not a regular file"),
            "{}",
            error.message
        );
        let big = dir.path().join("big.pem");
        let mut text = PKCS8_PLAIN.to_string();
        text.push_str(&"#".repeat(2 * 1024 * 1024));
        std::fs::write(&big, text)?;
        let error = load_private_key(&big, None)
            .err()
            .ok_or("a 2 MiB file was read")?;
        assert!(
            error.message.contains("larger than 1024 KiB"),
            "{}",
            error.message
        );
        Ok(())
    }

    /// Legacy OpenSSL encryption of `plain` with `passphrase` (AES-128-CBC).
    fn legacy_aes128(label: &str, plain: &[u8], passphrase: &str) -> String {
        use cbc::cipher::BlockModeEncrypt;
        let iv = [0x11u8; 16];
        let key = evp_bytes_to_key(passphrase.as_bytes(), &iv[..8], 16);
        let Ok(encryptor) = cbc::Encryptor::<aes::Aes128>::new_from_slices(&key, &iv) else {
            return String::new();
        };
        let data = encryptor.encrypt_padded_vec::<Pkcs7>(plain);
        format!(
            "-----BEGIN {label}-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,{}\n\n{}\n-----END {label}-----\n",
            "11".repeat(16),
            Base64::encode_string(&data)
        )
    }

    #[test]
    fn legacy_keys_must_decrypt_to_a_whole_key() -> Result<(), Box<dyn std::error::Error>> {
        // Unpads correctly and starts with a SEQUENCE, but is no key: what
        // a wrong passphrase gives about once in 65 000 tries.
        let fake = legacy_aes128(
            "EC PRIVATE KEY",
            &[0x30, 0x03, 0x02, 0x01, 0x01],
            "test-pass",
        );
        let error = parse_private_key(&fake, Some(&pass("test-pass")))
            .err()
            .ok_or("a non-key was accepted")?;
        assert_eq!(error.code.as_deref(), Some(PASSPHRASE_WRONG));
        // The real key in the same wrapping still loads.
        let sec1 = parse_private_key(SEC1_PLAIN, None)?;
        let wrapped = legacy_aes128("EC PRIVATE KEY", sec1.der(), "test-pass");
        assert_eq!(parse_private_key(&wrapped, Some(&pass("test-pass")))?, sec1);
        // A plain block that is no key is malformed, not a passphrase issue.
        let junk = format!(
            "-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----\n",
            Base64::encode_string(&[0x30, 0x03, 0x02, 0x01, 0x00, 0xff])
        );
        let error = parse_private_key(&junk, None)
            .err()
            .ok_or("junk accepted")?;
        assert_eq!(error.code, None);
        assert!(error.message.contains("malformed"), "{}", error.message);
        Ok(())
    }

    #[test]
    fn truncated_ciphertexts_are_corrupt_not_a_wrong_passphrase()
    -> Result<(), Box<dyn std::error::Error>> {
        // Legacy: 17 bytes is not a whole number of AES blocks.
        let truncated = format!(
            "-----BEGIN EC PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,{}\n\n{}\n-----END EC PRIVATE KEY-----\n",
            "11".repeat(16),
            Base64::encode_string(&[0x42; 17])
        );
        let error = parse_private_key(&truncated, Some(&pass("test-pass")))
            .err()
            .ok_or("accepted")?;
        assert_eq!(error.code, None, "{error:?}");
        assert!(error.message.contains("corrupt"), "{}", error.message);

        // PKCS#8: the encrypted data loses its last byte.
        let mut der = first_key_block(PKCS8_AES)?.der.to_vec();
        let octet = der
            .windows(3)
            .position(|w| w == [0x04, 0x81, 0x90])
            .ok_or("no encrypted data header")?;
        der.pop();
        if let Some(len) = der.get_mut(octet + 2) {
            *len -= 1;
        }
        if let Some(len) = der.get_mut(2) {
            *len -= 1;
        }
        let error = parse_private_key_der(&der, Some(&pass("test-pass")))
            .err()
            .ok_or("accepted")?;
        assert_eq!(error.code, None, "{error:?}");
        assert!(error.message.contains("corrupt"), "{}", error.message);
        Ok(())
    }

    #[tokio::test]
    async fn decrypted_keys_are_derived_once_and_forgotten_on_request()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let key = dir.path().join("cached.pem");
        std::fs::write(&key, PKCS8_SCRYPT)?;
        let key_path = key.display().to_string();
        let tls = TlsParams {
            mode: TlsMode::Require,
            client_cert_path: Some("/client.crt".into()),
            client_key_path: Some(key_path.clone()),
            client_key_passphrase: Some(pass("test-pass")),
            ..TlsParams::default()
        };
        let cached = |path: &str| cache().iter().filter(|e| e.path == Path::new(path)).count();
        load_client_identity(&tls, LONG).await?;
        assert_eq!(cached(&key_path), 1);
        let hits = |path: &str| {
            cache()
                .iter()
                .filter(|e| e.path == Path::new(path))
                .map(|e| e.hits)
                .sum::<u64>()
        };
        assert_eq!(hits(&key_path), 0);
        // The next sessions of the connect reuse it instead of deriving it.
        let again = load_client_identity(&tls, LONG).await?;
        assert_eq!(hits(&key_path), 1);
        assert_eq!(
            again.map(|i| i.key),
            Some(parse_private_key(PKCS8_PLAIN, None)?)
        );
        assert_eq!(cached(&key_path), 1);
        // Another passphrase never gets the cached key.
        let mut wrong = tls.clone();
        wrong.client_key_passphrase = Some(pass("nope"));
        let error = load_client_identity(&wrong, LONG).await.err();
        assert_eq!(
            error.and_then(|e| e.code).as_deref(),
            Some(PASSPHRASE_WRONG)
        );
        // Changed contents are derived again.
        std::fs::write(&key, PKCS8_AES)?;
        load_client_identity(&tls, LONG).await?;
        assert_eq!(cached(&key_path), 2);
        forget_cached_key(&key_path);
        assert_eq!(cached(&key_path), 0);
        Ok(())
    }

    #[tokio::test]
    async fn decrypting_gives_up_at_the_connect_timeout() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let key = dir.path().join("slow.pem");
        std::fs::write(&key, PKCS8_SCRYPT)?;
        let tls = TlsParams {
            mode: TlsMode::Require,
            client_cert_path: Some("/client.crt".into()),
            client_key_path: Some(key.display().to_string()),
            client_key_passphrase: Some(pass("test-pass")),
            ..TlsParams::default()
        };
        let error = load_client_identity(&tls, std::time::Duration::ZERO)
            .await
            .err()
            .ok_or("no timeout")?;
        assert_eq!(error.kind, ErrorKind::Timeout, "{error:?}");
        // The derivation goes on in the background; when it ends, its key
        // must not land in the cache (the connect is over).
        for _ in 0..600 {
            if finished().contains(&key) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(finished().contains(&key), "the derivation never finished");
        assert!(!is_key_cached(&key.display().to_string()));
        Ok(())
    }

    #[test]
    fn legacy_openssl_encryption_decrypts() -> Result<(), DriverError> {
        let ec = parse_private_key(SEC1_PLAIN, None)?;
        assert_eq!(
            parse_private_key(SEC1_AES128, Some(&pass("test-pass")))?,
            ec
        );
        let rsa = parse_private_key(RSA, None)?;
        for encrypted in [RSA_3DES, RSA_AES256] {
            assert_eq!(parse_private_key(encrypted, Some(&pass("test-pass")))?, rsa);
        }
        Ok(())
    }

    #[test]
    fn missing_and_wrong_passphrases_have_codes() {
        for encrypted in [
            PKCS8_AES,
            PKCS8_3DES,
            PKCS8_SCRYPT,
            PKCS8_PBKDF2_SHA1,
            SEC1_AES128,
            RSA_3DES,
            RSA_AES256,
        ] {
            let missing = parse_private_key(encrypted, None).err();
            assert_eq!(
                missing.and_then(|e| e.code).as_deref(),
                Some(PASSPHRASE_REQUIRED)
            );
            let wrong = parse_private_key(encrypted, Some(&pass("nope"))).err();
            assert_eq!(
                wrong.and_then(|e| e.code).as_deref(),
                Some(PASSPHRASE_WRONG)
            );
        }
    }

    #[test]
    fn errors_never_contain_the_passphrase() {
        let error = parse_private_key(RSA_AES256, Some(&pass("Sup3rS3cret"))).err();
        let text = format!("{error:?}");
        assert!(!text.contains("Sup3rS3cret"), "{text}");
        assert_eq!(
            format!("{:?}", parse_private_key(RSA, None).ok()),
            "Some(PrivateKeyDer::Pkcs1([redacted]))"
        );
    }

    #[test]
    fn pem_output_round_trips() -> Result<(), DriverError> {
        let key = parse_private_key(SEC1_AES128, Some(&pass("test-pass")))?;
        assert_eq!(parse_private_key(&key.to_pem(), None)?, key);
        Ok(())
    }

    #[test]
    fn single_des_and_garbage_are_refused() {
        let des = RSA_3DES.replace("DES-EDE3-CBC", "DES-CBC");
        let error = parse_private_key(&des, Some(&pass("test-pass"))).err();
        assert!(error.is_some_and(|e| e.message.contains("single DES")));
        assert!(parse_private_key("not a key", None).is_err());
        assert!(
            parse_private_key(
                "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
                None
            )
            .is_err()
        );
        assert!(parse_private_key("-----BEGIN PRIVATE KEY-----\nAAAA\n", None).is_err());
    }
}
