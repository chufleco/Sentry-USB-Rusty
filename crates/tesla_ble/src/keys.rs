//! Load the user's Tesla BLE NIST P-256 private key + derive the
//! public key for SessionInfoRequest. Also handles fresh key
//! generation, replacing the previous `tesla-keygen` shell-out.

use std::path::Path;

use anyhow::{Context, Result, bail};
use p256::SecretKey;
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};

/// Loaded ECDH keypair. The private key is for signing/ECDH; the
/// `pub_uncompressed` bytes are the 65-byte SEC1 format Tesla expects
/// in SessionInfoRequest (`0x04 || X || Y`).
pub struct KeyPair {
    pub secret: SecretKey,
    pub pub_uncompressed: Vec<u8>,
}

impl KeyPair {
    /// Read a Tesla BLE key file. Accepts both SEC1 PEM
    /// (`-----BEGIN EC PRIVATE KEY-----`, which is what tesla-keygen
    /// produces) and PKCS#8 PEM (`-----BEGIN PRIVATE KEY-----`).
    pub fn load(path: &Path) -> Result<Self> {
        let pem_str = std::fs::read_to_string(path)
            .with_context(|| format!("reading key file {}", path.display()))?;
        let parsed = pem::parse(&pem_str).context("parsing PEM envelope")?;
        let secret = match parsed.tag() {
            "EC PRIVATE KEY" => secret_from_sec1_der(parsed.contents())
                .context("parsing SEC1 DER EC private key")?,
            "PRIVATE KEY" => SecretKey::from_pkcs8_der(parsed.contents())
                .context("parsing PKCS#8 DER private key")?,
            other => bail!(
                "unexpected PEM type label {:?}; expected 'EC PRIVATE KEY' or 'PRIVATE KEY'",
                other
            ),
        };
        let pub_uncompressed = secret.public_key().to_sec1_bytes().as_ref().to_vec();
        Ok(Self {
            secret,
            pub_uncompressed,
        })
    }
}

/// Generate a fresh P-256 BLE keypair and write both halves to disk
/// at the standard Tesla locations.
///
/// Replaces the previous `tesla-keygen` shell-out. Writes:
///   * `<dir>/key_private.pem` — PKCS#8 PEM (`-----BEGIN PRIVATE KEY-----`).
///     Our `KeyPair::load` accepts both PKCS#8 and SEC1, so existing
///     installs (created by tesla-keygen, which uses SEC1) keep
///     working untouched; only freshly-generated keys land as PKCS#8.
///   * `<dir>/key_public.pem` — SubjectPublicKeyInfo PEM
///     (`-----BEGIN PUBLIC KEY-----`). Same format as tesla-keygen
///     so anything that still reads the public key file (tesla-control
///     add-key-request during pair, etc.) keeps working.
///
/// File permissions match tesla-keygen's conventions:
///   * private key: 0600 (owner-only read)
///   * public key:  0644
///
/// Returns the loaded keypair so the caller can use it immediately
/// without a separate `KeyPair::load`.
pub fn generate_keypair(dir: &Path) -> Result<KeyPair> {
    use p256::elliptic_curve::rand_core::OsRng;
    use p256::pkcs8::EncodePublicKey;

    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating key dir {}", dir.display()))?;

    let secret = SecretKey::random(&mut OsRng);
    let private_pem = secret
        .to_pkcs8_pem(LineEnding::LF)
        .context("encoding PKCS#8 private key")?;
    let public_pem = secret
        .public_key()
        .to_public_key_pem(LineEnding::LF)
        .context("encoding SPKI public key")?;

    let priv_path = dir.join("key_private.pem");
    let pub_path = dir.join("key_public.pem");
    std::fs::write(&priv_path, private_pem.as_bytes())
        .with_context(|| format!("writing {}", priv_path.display()))?;
    std::fs::write(&pub_path, public_pem.as_bytes())
        .with_context(|| format!("writing {}", pub_path.display()))?;

    // 0600 / 0644 — only matters on Unix; on other targets the chmod
    // is a no-op.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            &priv_path,
            std::fs::Permissions::from_mode(0o600),
        );
        let _ = std::fs::set_permissions(
            &pub_path,
            std::fs::Permissions::from_mode(0o644),
        );
    }

    KeyPair::load(&priv_path)
}

/// Hand-parse SEC1 ECPrivateKey DER to extract the 32-byte scalar.
/// p256 0.13 doesn't expose `from_sec1_pem`/`from_sec1_der` directly
/// under the feature set we use, so we walk the small fixed-shape
/// ASN.1 ourselves.
///
/// SEC1 layout (RFC 5915):
///   SEQUENCE {
///     INTEGER 1                              // version
///     OCTET STRING (32 bytes)                // privateKey
///     [0] OID 1.2.840.10045.3.1.7  OPTIONAL  // P-256 curve
///     [1] BIT STRING (uncompressed pubkey) OPTIONAL
///   }
fn secret_from_sec1_der(der: &[u8]) -> Result<SecretKey> {
    let mut i = 0;
    // Expect SEQUENCE
    if der.get(i) != Some(&0x30) {
        bail!("SEC1: expected SEQUENCE at offset 0");
    }
    i += 1;
    // Skip length bytes. ASN.1 length: if high bit set on first byte,
    // low bits are the count of further length bytes (we don't actually
    // care about the value, just how many to skip).
    let first_len = der.get(i).copied().context("SEC1: truncated length")?;
    if first_len & 0x80 == 0 {
        i += 1;
    } else {
        i += 1 + (first_len & 0x7f) as usize;
    }
    // Expect INTEGER 1 (`02 01 01`)
    if der.get(i..i + 3) != Some(&[0x02, 0x01, 0x01]) {
        bail!("SEC1: expected INTEGER version 1 at offset {}", i);
    }
    i += 3;
    // Expect OCTET STRING length 32 (`04 20`)
    if der.get(i..i + 2) != Some(&[0x04, 0x20]) {
        bail!("SEC1: expected 32-byte OCTET STRING at offset {}", i);
    }
    i += 2;
    let scalar = der
        .get(i..i + 32)
        .context("SEC1: truncated private key bytes")?;
    SecretKey::from_slice(scalar).context("invalid P-256 scalar")
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::SecretKey;
    use p256::elliptic_curve::rand_core::OsRng;
    use p256::pkcs8::EncodePrivateKey;

    #[test]
    fn round_trip_generated_pkcs8_key() {
        let key = SecretKey::random(&mut OsRng);
        let pem = key.to_pkcs8_pem(p256::pkcs8::LineEnding::LF).unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), pem.as_bytes()).unwrap();

        let loaded = KeyPair::load(tmp.path()).unwrap();
        assert_eq!(loaded.pub_uncompressed.len(), 65);
        assert_eq!(loaded.pub_uncompressed[0], 0x04);
    }

    #[test]
    fn generated_keypair_round_trips_through_disk() {
        // Generate into a tempdir; verify both files land + the
        // private key loads back via KeyPair::load + the loaded
        // pubkey matches the generated one.
        let dir = tempfile::tempdir().unwrap();
        let kp = generate_keypair(dir.path()).unwrap();

        let priv_path = dir.path().join("key_private.pem");
        let pub_path = dir.path().join("key_public.pem");
        assert!(priv_path.exists(), "private key file should exist");
        assert!(pub_path.exists(), "public key file should exist");

        let priv_pem = std::fs::read_to_string(&priv_path).unwrap();
        assert!(
            priv_pem.contains("-----BEGIN PRIVATE KEY-----"),
            "private key should be PKCS#8 PEM; got: {}",
            &priv_pem[..priv_pem.len().min(60)]
        );
        let pub_pem = std::fs::read_to_string(&pub_path).unwrap();
        assert!(
            pub_pem.contains("-----BEGIN PUBLIC KEY-----"),
            "public key should be SPKI PEM"
        );

        // Loader round-trip — confirms the file is a valid P-256 key.
        let loaded = KeyPair::load(&priv_path).unwrap();
        assert_eq!(
            loaded.pub_uncompressed.len(),
            65,
            "uncompressed SEC1 pubkey is 65 bytes"
        );
        assert_eq!(loaded.pub_uncompressed, kp.pub_uncompressed);
    }

    #[test]
    fn parses_sec1_pem_from_openssl() {
        // SEC1 PEM equivalent to the format `tesla-keygen` produces.
        // Generated via:
        //   openssl ecparam -name prime256v1 -genkey -noout
        // The exact bytes don't matter — just that the SEC1 path works.
        let pem = "-----BEGIN EC PRIVATE KEY-----\n\
                   MHcCAQEEIBnEX3tDgQHQX5IcAOA2RrvHV7ZzNeb7BLJ3vh7zVRpJoAoGCCqGSM49\n\
                   AwEHoUQDQgAEpUEnGcbqLEKMRwH69lcLN1H3xR/Mp3CY+QhBZkS1eOPF8Pdvkk0Q\n\
                   jiNAS/lZJaufnRu3WSjNu5xAvI4lNYjPiQ==\n\
                   -----END EC PRIVATE KEY-----\n";
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), pem).unwrap();

        let loaded = KeyPair::load(tmp.path()).unwrap();
        assert_eq!(loaded.pub_uncompressed.len(), 65);
        assert_eq!(loaded.pub_uncompressed[0], 0x04);
    }
}
