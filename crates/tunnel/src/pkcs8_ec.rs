//! PKCS#8 ECDSA keys with explicit curve parameters.
//!
//! `ssh-keygen -m PKCS8` linked against LibreSSL (the macOS system build)
//! writes ECDSA keys whose algorithm identifier carries the full curve
//! parameters instead of a named curve OID. The SSH library reads explicit
//! parameters only from SEC1 (`BEGIN EC PRIVATE KEY`) files, so such a
//! PKCS#8 key is rewritten here into the equivalent SEC1 structure: the
//! inner `ECPrivateKey` with the curve parameters moved into its
//! `parameters [0]` field. Nothing else is changed.

use data_encoding::BASE64_MIME_PERMISSIVE;
use pkcs8::EncryptedPrivateKeyInfoRef;
use pkcs8::der::Decode;
use zeroize::Zeroizing;

const TAG_INTEGER: u8 = 0x02;
const TAG_OCTET_STRING: u8 = 0x04;
const TAG_OID: u8 = 0x06;
const TAG_SEQUENCE: u8 = 0x30;
const TAG_EC_PARAMETERS: u8 = 0xA0;

/// DER content of the OID `1.2.840.10045.2.1` (id-ecPublicKey).
const ID_EC_PUBLIC_KEY: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];

/// Result of the rewrite.
pub(crate) enum Rewrite {
    /// Not a PKCS#8 ECDSA key with explicit parameters.
    NotApplicable,
    /// The passphrase does not decrypt the key.
    WrongPassphrase,
    /// The equivalent SEC1 PEM text.
    Sec1(Zeroizing<String>),
}

/// Rewrites `BEGIN PRIVATE KEY` or `BEGIN ENCRYPTED PRIVATE KEY` text.
pub(crate) fn rewrite(text: &str, password: Option<&str>) -> Rewrite {
    let (der, encrypted) = if let Some(body) = pem_body(text, "PRIVATE KEY") {
        (body, false)
    } else if let Some(body) = pem_body(text, "ENCRYPTED PRIVATE KEY") {
        (body, true)
    } else {
        return Rewrite::NotApplicable;
    };
    let der = if encrypted {
        let Some(password) = password else {
            return Rewrite::NotApplicable;
        };
        let Ok(info) = EncryptedPrivateKeyInfoRef::from_der(&der) else {
            return Rewrite::NotApplicable;
        };
        match info.decrypt(password.as_bytes()) {
            Ok(doc) => Zeroizing::new(doc.as_bytes().to_vec()),
            Err(_) => return Rewrite::WrongPassphrase,
        }
    } else {
        der
    };
    match pkcs8_to_sec1(&der) {
        Some(sec1) => Rewrite::Sec1(to_pem(&sec1, "EC PRIVATE KEY")),
        None => Rewrite::NotApplicable,
    }
}

fn pkcs8_to_sec1(der: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
    // PrivateKeyInfo ::= SEQUENCE { version, AlgorithmIdentifier, OCTET STRING, ... }
    let ((tag, content, _), rest) = read_tlv(der)?;
    if tag != TAG_SEQUENCE || !rest.is_empty() {
        return None;
    }
    let fields = children(content)?;
    let [version, algorithm, private_key, ..] = fields.as_slice() else {
        return None;
    };
    if version.0 != TAG_INTEGER || algorithm.0 != TAG_SEQUENCE || private_key.0 != TAG_OCTET_STRING
    {
        return None;
    }
    // AlgorithmIdentifier ::= SEQUENCE { id-ecPublicKey, ECParameters }
    let algorithm = children(algorithm.1)?;
    let [oid, params] = algorithm.as_slice() else {
        return None;
    };
    if oid.0 != TAG_OID || oid.1 != ID_EC_PUBLIC_KEY || params.0 != TAG_SEQUENCE {
        // Named curves are handled by the SSH library itself.
        return None;
    }
    // ECPrivateKey ::= SEQUENCE { version, privateKey OCTET STRING,
    //                             parameters [0] OPTIONAL, publicKey [1] OPTIONAL }
    let ((tag, inner, _), rest) = read_tlv(private_key.1)?;
    if tag != TAG_SEQUENCE || !rest.is_empty() {
        return None;
    }
    let inner = children(inner)?;
    if inner.iter().any(|f| f.0 == TAG_EC_PARAMETERS) {
        return None;
    }
    let (head, tail) = inner.split_at_checked(2)?;
    let mut body = Zeroizing::new(Vec::new());
    for field in head {
        body.extend_from_slice(field.2);
    }
    body.extend_from_slice(&tlv(TAG_EC_PARAMETERS, params.2));
    for field in tail {
        body.extend_from_slice(field.2);
    }
    Some(Zeroizing::new(tlv(TAG_SEQUENCE, &body)))
}

/// One DER element: tag, content, the whole encoding.
type Field<'a> = (u8, &'a [u8], &'a [u8]);

/// Reads one element with a single-byte tag; returns it and the rest.
fn read_tlv(input: &[u8]) -> Option<(Field<'_>, &[u8])> {
    let (&tag, after_tag) = input.split_first()?;
    let (&first, mut after_len) = after_tag.split_first()?;
    let len = if first < 0x80 {
        usize::from(first)
    } else {
        let count = usize::from(first & 0x7F);
        if count == 0 || count > 4 {
            return None;
        }
        let (bytes, rest) = after_len.split_at_checked(count)?;
        after_len = rest;
        bytes
            .iter()
            .fold(0usize, |acc, &b| (acc << 8) | usize::from(b))
    };
    let (content, rest) = after_len.split_at_checked(len)?;
    let header = input.len() - after_len.len();
    let whole = input.get(..header + len)?;
    Some(((tag, content, whole), rest))
}

fn children(mut content: &[u8]) -> Option<Vec<Field<'_>>> {
    let mut out = Vec::new();
    while !content.is_empty() {
        let ((tag, value, whole), rest) = read_tlv(content)?;
        out.push((tag, value, whole));
        content = rest;
    }
    Some(out)
}

fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        let significant = bytes.get(skip..).unwrap_or_default();
        out.push(0x80 | significant.len() as u8);
        out.extend_from_slice(significant);
    }
    out.extend_from_slice(content);
    out
}

/// The decoded body of a PEM block with exactly this label.
fn pem_body(text: &str, label: &str) -> Option<Zeroizing<Vec<u8>>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = text.find(&begin)? + begin.len();
    let stop = start + text.get(start..)?.find(&end)?;
    let body = text.get(start..stop)?;
    BASE64_MIME_PERMISSIVE
        .decode(body.trim().as_bytes())
        .ok()
        .map(Zeroizing::new)
}

fn to_pem(der: &[u8], label: &str) -> Zeroizing<String> {
    let encoded = Zeroizing::new(data_encoding::BASE64.encode(der));
    let mut out = Zeroizing::new(format!("-----BEGIN {label}-----\n"));
    for chunk in encoded.as_bytes().chunks(64) {
        out.push_str(&String::from_utf8_lossy(chunk));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn der_lengths_round_trip() {
        for len in [0usize, 1, 127, 128, 255, 256, 65_535, 70_000] {
            let content = vec![0xAB; len];
            let encoded = tlv(TAG_OCTET_STRING, &content);
            let ((tag, value, whole), rest) = read_tlv(&encoded).unwrap_or_default();
            assert_eq!(tag, TAG_OCTET_STRING);
            assert_eq!(value.len(), len);
            assert_eq!(whole.len(), encoded.len());
            assert!(rest.is_empty());
        }
        assert!(read_tlv(&[0x04, 0x85, 1, 2, 3, 4, 5]).is_none());
        assert!(read_tlv(&[0x04, 0x05, 1]).is_none());
    }

    #[test]
    fn other_texts_are_left_alone() {
        assert!(matches!(rewrite("hello", None), Rewrite::NotApplicable));
        assert!(matches!(
            rewrite(
                "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
                None
            ),
            Rewrite::NotApplicable
        ));
    }
}
