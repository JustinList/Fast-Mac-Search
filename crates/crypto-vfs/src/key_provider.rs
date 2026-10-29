use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use hkdf::Hkdf;
use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::CryptoVfsError;

/// 256-bit root master key held only in wired/zeroize-on-drop memory.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct MasterKey([u8; 32]);

impl MasterKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn random() -> Self {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Derives an isolated 256-bit Data Encryption Key (DEK) for a specific volume shard
    /// using HKDF-SHA256 bound to the hardware Volume UUID and per-shard salt.
    pub fn derive_shard_key(&self, volume_uuid: &[u8; 16], shard_salt: &[u8; 16]) -> ShardKey {
        let hk = Hkdf::<Sha256>::new(Some(shard_salt), &self.0);
        let mut info = [0u8; 34];
        info[..18].copy_from_slice(b"fms-shard-dek-v1::");
        info[18..34].copy_from_slice(volume_uuid);

        let mut dek = [0u8; 32];
        hk.expand(&info, &mut dek)
            .expect("32-byte HKDF-SHA256 output is always valid length");
        ShardKey(dek)
    }
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MasterKey([REDACTED 256-BIT])")
    }
}

/// 256-bit per-shard Data Encryption Key (DEK) derived via HKDF-SHA256.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct ShardKey([u8; 32]);

impl ShardKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn random_for_tests() -> Self {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for ShardKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ShardKey([REDACTED 256-BIT])")
    }
}

/// Describes which key-protection tier unlocked the MasterKey.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyBackendKind {
    /// Hardware Apple Silicon Secure Enclave P-256 ECIES key-wrapping (signed app bundle).
    SecureEnclaveEcies,
    /// macOS Login Keychain item (`Security.framework`).
    MacOsLoginKeychain,
    /// User passphrase via Argon2id (Portable Encrypted `.fastindex` mode or `FMS_PASSPHRASE`).
    Argon2idPassphrase,
    /// Restricted `0600` local developer keyfile fallback for unsigned OSS / CI builds.
    LocalSealedKeyfile,
}

impl fmt::Display for KeyBackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SecureEnclaveEcies => write!(f, "Apple Secure Enclave (P-256 ECIES)"),
            Self::MacOsLoginKeychain => write!(f, "macOS Login Keychain"),
            Self::Argon2idPassphrase => write!(f, "Argon2id Passphrase"),
            Self::LocalSealedKeyfile => write!(f, "Local Sealed Keyfile (0600 OSS fallback)"),
        }
    }
}

/// Metadata stored alongside a shard (`shard_envelope.json`) containing only non-secret KDF parameters
/// or ECIES wrapped key material when wrapped by Secure Enclave.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardKeyEnvelope {
    pub version: u8,
    pub volume_uuid_hex: String,
    pub shard_salt_hex: String,
    pub argon2_salt_hex: Option<String>,
    pub backend_hint: KeyBackendKind,
}

impl ShardKeyEnvelope {
    pub fn new(volume_uuid: &[u8; 16], backend_hint: KeyBackendKind) -> (Self, [u8; 16], [u8; 16]) {
        let mut shard_salt = [0u8; 16];
        let mut argon2_salt = [0u8; 16];
        OsRng.fill_bytes(&mut shard_salt);
        OsRng.fill_bytes(&mut argon2_salt);
        (
            Self {
                version: 1,
                volume_uuid_hex: hex_encode(volume_uuid),
                shard_salt_hex: hex_encode(&shard_salt),
                argon2_salt_hex: Some(hex_encode(&argon2_salt)),
                backend_hint,
            },
            shard_salt,
            argon2_salt,
        )
    }

    pub fn shard_salt(&self) -> Result<[u8; 16], CryptoVfsError> {
        hex_decode_16(&self.shard_salt_hex)
    }

    pub fn argon2_salt(&self) -> Result<Option<[u8; 16]>, CryptoVfsError> {
        match &self.argon2_salt_hex {
            Some(s) => Ok(Some(hex_decode_16(s)?)),
            None => Ok(None),
        }
    }
}

/// Derives a 256-bit `MasterKey` from a passphrase and 16-byte salt using Argon2id.
pub fn derive_master_key_from_passphrase(
    passphrase: &[u8],
    salt: &[u8; 16],
) -> Result<MasterKey, CryptoVfsError> {
    // Interactive-fast yet GPU-hard Argon2id parameters: 19 MiB RAM, 2 iterations, 4 lanes (~25ms on M-series)
    let params = Params::new(19 * 1024, 2, 4, Some(32))
        .map_err(|e| CryptoVfsError::KeyDerivation(e.to_string()))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; 32];
    argon2
        .hash_password_into(passphrase, salt, &mut out)
        .map_err(|e| CryptoVfsError::KeyDerivation(e.to_string()))?;
    Ok(MasterKey::from_bytes(out))
}

/// Status of probing the Apple Silicon Secure Enclave on the current process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecureEnclaveAvailability {
    Available,
    MissingEntitlement,
    Unavailable(i32),
}

/// Hybrid KeyProvider supporting:
/// 1. Explicit Passphrase (`Argon2id`) for Portable `.fastindex` external SSD shards
/// 2. Apple Secure Enclave (`P-256 ECIES` key wrapping) when code-signed with Apple entitlements
/// 3. macOS Login Keychain non-interactive lookup (`Security.framework`) when signed
/// 4. Local `0600` sealed keyfile fallback for unsigned `cargo run` / GitHub contributors
pub struct HybridKeyProvider {
    key_storage_dir: PathBuf,
    allow_keychain: bool,
}

impl HybridKeyProvider {
    /// Creates a `HybridKeyProvider`.
    ///
    /// Note on macOS Developer UX: Ad-hoc / unsigned `cargo build` binaries change their CDHash on
    /// every recompile, which causes macOS `SecurityAgent` to pop a modal password prompt for
    /// legacy Login Keychain items (`dev.fastmacsearch.masterkey.v1`). Therefore, Keychain / Secure
    /// Enclave lookup defaults to opt-in (`FMS_USE_KEYCHAIN=1` or `.with_keychain(true)` in signed
    /// `.app` bundles), using the `0600` owner-only local sealed keyfile (`keys/master.key`) otherwise.
    pub fn new(key_storage_dir: impl Into<PathBuf>) -> Self {
        let env_opt_in = std::env::var("FMS_USE_KEYCHAIN")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        Self {
            key_storage_dir: key_storage_dir.into(),
            allow_keychain: env_opt_in,
        }
    }

    pub fn with_keychain(mut self, allow_keychain: bool) -> Self {
        self.allow_keychain = allow_keychain;
        self
    }

    /// Probes whether this binary currently holds the code-signing entitlement required by
    /// `kSecAttrTokenIDSecureEnclave` (`errSecMissingEntitlement = -34018` when unsigned).
    pub fn probe_secure_enclave(&self) -> SecureEnclaveAvailability {
        #[cfg(target_os = "macos")]
        {
            macos_security::probe_secure_enclave_entitlement()
        }
        #[cfg(not(target_os = "macos"))]
        {
            SecureEnclaveAvailability::Unavailable(-1)
        }
    }

    /// Resolves (or initializes) the root 256-bit `MasterKey` along with the active backend tier.
    pub fn resolve_master_key(
        &self,
        passphrase: Option<&[u8]>,
        argon2_salt: Option<&[u8; 16]>,
    ) -> Result<(MasterKey, KeyBackendKind), CryptoVfsError> {
        // Tier 0: Explicit passphrase (e.g. Portable Encrypted Mode on external drive or FMS_PASSPHRASE)
        if let Some(secret) = passphrase {
            let salt = match argon2_salt {
                Some(s) => *s,
                None => self.load_or_create_passphrase_salt()?,
            };
            let key = derive_master_key_from_passphrase(secret, &salt)?;
            return Ok((key, KeyBackendKind::Argon2idPassphrase));
        }

        // Tier 1: Apple Silicon Secure Enclave ECIES-wrapped master key (when code-signed with SE entitlement)
        #[cfg(target_os = "macos")]
        if self.allow_keychain {
            let wrapped_path = self.key_storage_dir.join("master.se.ecies");
            if let Ok(Some(mk)) = macos_security::try_secure_enclave_master_key(&wrapped_path) {
                return Ok((mk, KeyBackendKind::SecureEnclaveEcies));
            }

            // Tier 2: Non-interactive macOS Login Keychain (never pops a blocking GUI dialog in CLI)
            if let Ok(Some(mk)) = macos_security::try_login_keychain_master_key() {
                return Ok((mk, KeyBackendKind::MacOsLoginKeychain));
            }
        }

        // Tier 3: Sealed 0600 local developer keyfile (works out-of-the-box for unsigned `cargo run` / OSS)
        let mk = self.load_or_create_local_keyfile()?;
        Ok((mk, KeyBackendKind::LocalSealedKeyfile))
    }

    /// Resolves the per-shard `ShardKey` for a shard directory, creating `shard_envelope.json` if new.
    pub fn resolve_or_init_shard_key(
        &self,
        shard_dir: &Path,
        volume_uuid: &[u8; 16],
        passphrase: Option<&[u8]>,
    ) -> Result<(ShardKey, KeyBackendKind, ShardKeyEnvelope), CryptoVfsError> {
        fs::create_dir_all(shard_dir)?;
        let envelope_path = shard_dir.join("shard_envelope.json");

        if envelope_path.exists() {
            let raw = fs::read_to_string(&envelope_path)?;
            let envelope: ShardKeyEnvelope = serde_json::from_str(&raw)
                .map_err(|e| CryptoVfsError::CorruptHeader(format!("invalid shard_envelope.json: {e}")))?;
            let shard_salt = envelope.shard_salt()?;
            let a2_salt = envelope.argon2_salt()?;
            // Respect `self.allow_keychain` strictly so ad-hoc / unsigned builds never touch
            // legacy Login Keychain items (`dev.fastmacsearch.masterkey.v1`) or trigger GUI prompts.
            let (mk, backend) = self.resolve_master_key(passphrase, a2_salt.as_ref())?;
            let dek = mk.derive_shard_key(volume_uuid, &shard_salt);
            Ok((dek, backend, envelope))
        } else {
            let mut probe_a2_salt = [0u8; 16];
            OsRng.fill_bytes(&mut probe_a2_salt);
            let (mk, backend) = self.resolve_master_key(passphrase, Some(&probe_a2_salt))?;
            let (mut envelope, shard_salt, _) = ShardKeyEnvelope::new(volume_uuid, backend);
            envelope.argon2_salt_hex = Some(hex_encode(&probe_a2_salt));

            let serialized = serde_json::to_vec_pretty(&envelope)
                .map_err(|e| CryptoVfsError::CorruptHeader(e.to_string()))?;
            let tmp_path = shard_dir.join("shard_envelope.json.tmp");
            write_restricted_file(&tmp_path, &serialized)?;
            fs::rename(&tmp_path, &envelope_path)?;

            let dek = mk.derive_shard_key(volume_uuid, &shard_salt);
            Ok((dek, backend, envelope))
        }
    }

    fn load_or_create_passphrase_salt(&self) -> Result<[u8; 16], CryptoVfsError> {
        fs::create_dir_all(&self.key_storage_dir)?;
        let salt_path = self.key_storage_dir.join("argon2.salt");
        if salt_path.exists() {
            let bytes = fs::read(&salt_path)?;
            if bytes.len() == 16 {
                let mut salt = [0u8; 16];
                salt.copy_from_slice(&bytes);
                return Ok(salt);
            }
        }
        let mut salt = [0u8; 16];
        OsRng.fill_bytes(&mut salt);
        write_restricted_file(&salt_path, &salt)?;
        Ok(salt)
    }

    fn load_or_create_local_keyfile(&self) -> Result<MasterKey, CryptoVfsError> {
        fs::create_dir_all(&self.key_storage_dir)?;
        let key_path = self.key_storage_dir.join("master.key");
        if key_path.exists() {
            let mut f = fs::File::open(&key_path)?;
            let mut buf = [0u8; 32];
            f.read_exact(&mut buf)?;
            let mk = MasterKey::from_bytes(buf);
            buf.zeroize();
            return Ok(mk);
        }

        let mk = MasterKey::random();
        let tmp_path = self.key_storage_dir.join("master.key.tmp");
        write_restricted_file(&tmp_path, mk.as_bytes())?;
        fs::rename(&tmp_path, &key_path)?;
        Ok(mk)
    }
}

fn write_restricted_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(data)?;
    file.sync_all()
}

pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

pub fn hex_decode_16(s: &str) -> Result<[u8; 16], CryptoVfsError> {
    if s.len() != 32 {
        return Err(CryptoVfsError::CorruptHeader(format!(
            "expected 32 hex chars, got {}",
            s.len()
        )));
    }
    let mut out = [0u8; 16];
    for (i, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_nibble(c: u8) -> Result<u8, CryptoVfsError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(CryptoVfsError::CorruptHeader("invalid hex digit".into())),
    }
}

#[cfg(target_os = "macos")]
mod macos_security {
    use std::ffi::c_void;
    use std::fs;
    use std::path::Path;
    use std::ptr;

    use zeroize::Zeroize;

    use super::{MasterKey, SecureEnclaveAvailability, write_restricted_file};
    use crate::CryptoVfsError;

    type CFTypeRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFMutableDictionaryRef = *mut c_void;
    type CFStringRef = *const c_void;
    type CFDataRef = *const c_void;
    type CFNumberRef = *const c_void;
    type CFBooleanRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type CFErrorRef = *mut c_void;
    type SecKeyRef = *mut c_void;
    type OSStatus = i32;

    const ERR_SEC_SUCCESS: OSStatus = 0;
    const ERR_SEC_ITEM_NOT_FOUND: OSStatus = -25300;
    const ERR_SEC_MISSING_ENTITLEMENT: OSStatus = -34018;
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const K_CF_NUMBER_SINT32_TYPE: isize = 3;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFBooleanTrue: CFBooleanRef;
        static kCFTypeDictionaryKeyCallBacks: c_void;
        static kCFTypeDictionaryValueCallBacks: c_void;

        fn CFDictionaryCreateMutable(
            allocator: CFAllocatorRef,
            capacity: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> CFMutableDictionaryRef;
        fn CFDictionarySetValue(dict: CFMutableDictionaryRef, key: *const c_void, value: *const c_void);
        fn CFStringCreateWithBytes(
            alloc: CFAllocatorRef,
            bytes: *const u8,
            num_bytes: isize,
            encoding: u32,
            is_external: u8,
        ) -> CFStringRef;
        fn CFDataCreate(alloc: CFAllocatorRef, bytes: *const u8, length: isize) -> CFDataRef;
        fn CFDataGetLength(the_data: CFDataRef) -> isize;
        fn CFDataGetBytePtr(the_data: CFDataRef) -> *const u8;
        fn CFNumberCreate(
            allocator: CFAllocatorRef,
            the_type: isize,
            value_ptr: *const c_void,
        ) -> CFNumberRef;
        fn CFRelease(cf: CFTypeRef);
    }

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecClass: CFStringRef;
        static kSecClassKey: CFStringRef;
        static kSecClassGenericPassword: CFStringRef;
        static kSecAttrKeyType: CFStringRef;
        static kSecAttrKeyTypeECSECPrimeRandom: CFStringRef;
        static kSecAttrKeySizeInBits: CFStringRef;
        static kSecAttrTokenID: CFStringRef;
        static kSecAttrTokenIDSecureEnclave: CFStringRef;
        static kSecAttrApplicationTag: CFStringRef;
        static kSecAttrIsPermanent: CFStringRef;
        static kSecPrivateKeyAttrs: CFStringRef;
        static kSecReturnRef: CFStringRef;
        static kSecReturnData: CFStringRef;
        static kSecAttrService: CFStringRef;
        static kSecAttrAccount: CFStringRef;
        static kSecValueData: CFStringRef;
        static kSecUseAuthenticationUI: CFStringRef;
        static kSecUseAuthenticationUIFail: CFStringRef;
        static kSecKeyAlgorithmECIESEncryptionCofactorVariableIVX963SHA256AESGCM: CFStringRef;

        fn SecItemCopyMatching(query: CFDictionaryRef, result: *mut CFTypeRef) -> OSStatus;
        fn SecItemAdd(attributes: CFDictionaryRef, result: *mut CFTypeRef) -> OSStatus;
        fn SecKeychainSetUserInteractionAllowed(state: u8) -> OSStatus;
        fn SecKeyCreateRandomKey(parameters: CFDictionaryRef, error: *mut CFErrorRef) -> SecKeyRef;
        fn SecKeyCopyPublicKey(key: SecKeyRef) -> SecKeyRef;
        fn SecKeyCreateEncryptedData(
            key: SecKeyRef,
            algorithm: CFStringRef,
            plaintext: CFDataRef,
            error: *mut CFErrorRef,
        ) -> CFDataRef;
        fn SecKeyCreateDecryptedData(
            key: SecKeyRef,
            algorithm: CFStringRef,
            ciphertext: CFDataRef,
            error: *mut CFErrorRef,
        ) -> CFDataRef;
    }

    const SE_TAG: &[u8] = b"dev.fastmacsearch.se.masterwrap.v1";
    const KC_SERVICE: &[u8] = b"dev.fastmacsearch.masterkey.v1";
    const KC_ACCOUNT: &[u8] = b"default";

    struct CfGuard(CFTypeRef);
    impl Drop for CfGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) };
            }
        }
    }

    unsafe fn make_cf_dict() -> CFMutableDictionaryRef {
        unsafe {
            CFDictionaryCreateMutable(
                ptr::null(),
                0,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        }
    }

    unsafe fn make_cf_string(bytes: &[u8]) -> CFStringRef {
        unsafe {
            CFStringCreateWithBytes(
                ptr::null(),
                bytes.as_ptr(),
                bytes.len() as isize,
                K_CF_STRING_ENCODING_UTF8,
                0,
            )
        }
    }

    /// Probes whether the current binary has Data Protection / Secure Enclave entitlements.
    pub fn probe_secure_enclave_entitlement() -> SecureEnclaveAvailability {
        unsafe {
            let query = make_cf_dict();
            let _q_guard = CfGuard(query as CFTypeRef);
            let tag = CFDataCreate(ptr::null(), SE_TAG.as_ptr(), SE_TAG.len() as isize);
            let _tag_guard = CfGuard(tag);

            CFDictionarySetValue(query, kSecClass as _, kSecClassKey as _);
            CFDictionarySetValue(query, kSecAttrTokenID as _, kSecAttrTokenIDSecureEnclave as _);
            CFDictionarySetValue(query, kSecAttrApplicationTag as _, tag as _);
            CFDictionarySetValue(query, kSecUseAuthenticationUI as _, kSecUseAuthenticationUIFail as _);

            let mut out: CFTypeRef = ptr::null();
            let status = SecItemCopyMatching(query, &mut out);
            if !out.is_null() {
                CFRelease(out);
            }
            match status {
                ERR_SEC_SUCCESS | ERR_SEC_ITEM_NOT_FOUND => SecureEnclaveAvailability::Available,
                ERR_SEC_MISSING_ENTITLEMENT => SecureEnclaveAvailability::MissingEntitlement,
                other => SecureEnclaveAvailability::Unavailable(other),
            }
        }
    }

    /// Attempts to wrap/unwrap the 256-bit MasterKey using an Apple Silicon Secure Enclave
    /// P-256 hardware key via ECIES (`CofactorVariableIVX963SHA256AESGCM`).
    pub fn try_secure_enclave_master_key(
        wrapped_blob_path: &Path,
    ) -> Result<Option<MasterKey>, CryptoVfsError> {
        if probe_secure_enclave_entitlement() != SecureEnclaveAvailability::Available {
            return Ok(None);
        }

        unsafe {
            let priv_key = match lookup_or_create_se_key() {
                Some(k) => k,
                None => return Ok(None),
            };
            let _priv_guard = CfGuard(priv_key as CFTypeRef);

            if wrapped_blob_path.exists() {
                let ciphertext = fs::read(wrapped_blob_path)?;
                let cf_cipher = CFDataCreate(
                    ptr::null(),
                    ciphertext.as_ptr(),
                    ciphertext.len() as isize,
                );
                let _c_guard = CfGuard(cf_cipher);
                let mut err: CFErrorRef = ptr::null_mut();
                let plain_cf = SecKeyCreateDecryptedData(
                    priv_key,
                    kSecKeyAlgorithmECIESEncryptionCofactorVariableIVX963SHA256AESGCM,
                    cf_cipher,
                    &mut err,
                );
                if !err.is_null() {
                    CFRelease(err as CFTypeRef);
                    return Ok(None);
                }
                if plain_cf.is_null() {
                    return Ok(None);
                }
                let _p_guard = CfGuard(plain_cf);
                if CFDataGetLength(plain_cf) != 32 {
                    return Ok(None);
                }
                let mut key_bytes = [0u8; 32];
                ptr::copy_nonoverlapping(CFDataGetBytePtr(plain_cf), key_bytes.as_mut_ptr(), 32);
                let mk = MasterKey::from_bytes(key_bytes);
                key_bytes.zeroize();
                return Ok(Some(mk));
            }

            // Generate a fresh 256-bit MasterKey and wrap it with the Secure Enclave P-256 public key
            let pub_key = SecKeyCopyPublicKey(priv_key);
            if pub_key.is_null() {
                return Ok(None);
            }
            let _pub_guard = CfGuard(pub_key as CFTypeRef);

            let mk = MasterKey::random();
            let cf_plain = CFDataCreate(ptr::null(), mk.as_bytes().as_ptr(), 32);
            let _pl_guard = CfGuard(cf_plain);
            let mut err: CFErrorRef = ptr::null_mut();
            let cipher_cf = SecKeyCreateEncryptedData(
                pub_key,
                kSecKeyAlgorithmECIESEncryptionCofactorVariableIVX963SHA256AESGCM,
                cf_plain,
                &mut err,
            );
            if !err.is_null() {
                CFRelease(err as CFTypeRef);
                return Ok(None);
            }
            if cipher_cf.is_null() {
                return Ok(None);
            }
            let _ci_guard = CfGuard(cipher_cf);
            let len = CFDataGetLength(cipher_cf) as usize;
            let slice = std::slice::from_raw_parts(CFDataGetBytePtr(cipher_cf), len);
            if let Some(parent) = wrapped_blob_path.parent() {
                fs::create_dir_all(parent)?;
            }
            write_restricted_file(wrapped_blob_path, slice)?;
            Ok(Some(mk))
        }
    }

    unsafe fn lookup_or_create_se_key() -> Option<SecKeyRef> {
        unsafe {
            let tag = CFDataCreate(ptr::null(), SE_TAG.as_ptr(), SE_TAG.len() as isize);
            let _tag_guard = CfGuard(tag);

            let query = make_cf_dict();
            let _q_guard = CfGuard(query as CFTypeRef);
            CFDictionarySetValue(query, kSecClass as _, kSecClassKey as _);
            CFDictionarySetValue(query, kSecAttrTokenID as _, kSecAttrTokenIDSecureEnclave as _);
            CFDictionarySetValue(query, kSecAttrApplicationTag as _, tag as _);
            CFDictionarySetValue(query, kSecAttrKeyType as _, kSecAttrKeyTypeECSECPrimeRandom as _);
            CFDictionarySetValue(query, kSecReturnRef as _, kCFBooleanTrue as _);
            CFDictionarySetValue(query, kSecUseAuthenticationUI as _, kSecUseAuthenticationUIFail as _);

            let mut existing: CFTypeRef = ptr::null();
            let status = SecItemCopyMatching(query, &mut existing);
            if status == ERR_SEC_SUCCESS && !existing.is_null() {
                return Some(existing as SecKeyRef);
            }

            let priv_attrs = make_cf_dict();
            let _pa_guard = CfGuard(priv_attrs as CFTypeRef);
            CFDictionarySetValue(priv_attrs, kSecAttrIsPermanent as _, kCFBooleanTrue as _);
            CFDictionarySetValue(priv_attrs, kSecAttrApplicationTag as _, tag as _);

            let bits: i32 = 256;
            let cf_bits = CFNumberCreate(
                ptr::null(),
                K_CF_NUMBER_SINT32_TYPE,
                (&raw const bits).cast(),
            );
            let _bits_guard = CfGuard(cf_bits);

            let params = make_cf_dict();
            let _pm_guard = CfGuard(params as CFTypeRef);
            CFDictionarySetValue(params, kSecAttrKeyType as _, kSecAttrKeyTypeECSECPrimeRandom as _);
            CFDictionarySetValue(params, kSecAttrKeySizeInBits as _, cf_bits as _);
            CFDictionarySetValue(params, kSecAttrTokenID as _, kSecAttrTokenIDSecureEnclave as _);
            CFDictionarySetValue(params, kSecPrivateKeyAttrs as _, priv_attrs as _);

            let mut err: CFErrorRef = ptr::null_mut();
            let created = SecKeyCreateRandomKey(params, &mut err);
            if !err.is_null() {
                CFRelease(err as CFTypeRef);
                return None;
            }
            if created.is_null() {
                None
            } else {
                Some(created)
            }
        }
    }

    /// Reads an existing non-interactive generic password item from the macOS Login Keychain
    /// if previously provisioned by a signed build. Explicitly calls
    /// `SecKeychainSetUserInteractionAllowed(0)` in addition to `kSecUseAuthenticationUIFail`
    /// so `SecurityAgent` is strictly forbidden from popping modal password boxes on legacy keychains.
    pub fn try_login_keychain_master_key() -> Result<Option<MasterKey>, CryptoVfsError> {
        unsafe {
            let _ = SecKeychainSetUserInteractionAllowed(0);
            let service = make_cf_string(KC_SERVICE);
            let _s_guard = CfGuard(service);
            let account = make_cf_string(KC_ACCOUNT);
            let _a_guard = CfGuard(account);

            let query = make_cf_dict();
            let _q_guard = CfGuard(query as CFTypeRef);
            CFDictionarySetValue(query, kSecClass as _, kSecClassGenericPassword as _);
            CFDictionarySetValue(query, kSecAttrService as _, service as _);
            CFDictionarySetValue(query, kSecAttrAccount as _, account as _);
            CFDictionarySetValue(query, kSecReturnData as _, kCFBooleanTrue as _);
            CFDictionarySetValue(query, kSecUseAuthenticationUI as _, kSecUseAuthenticationUIFail as _);

            let mut result: CFTypeRef = ptr::null();
            let status = SecItemCopyMatching(query, &mut result);
            if status == ERR_SEC_SUCCESS && !result.is_null() {
                let _r_guard = CfGuard(result);
                let data_ref = result as CFDataRef;
                if CFDataGetLength(data_ref) == 32 {
                    let mut buf = [0u8; 32];
                    ptr::copy_nonoverlapping(CFDataGetBytePtr(data_ref), buf.as_mut_ptr(), 32);
                    let mk = MasterKey::from_bytes(buf);
                    buf.zeroize();
                    return Ok(Some(mk));
                }
            } else if status == ERR_SEC_ITEM_NOT_FOUND {
                // Only insert automatically when code-signed with entitlements so recompiling
                // unsigned dev binaries doesn't create per-binary ACL prompts.
                if probe_secure_enclave_entitlement() == SecureEnclaveAvailability::Available {
                    let mk = MasterKey::random();
                    let val = CFDataCreate(ptr::null(), mk.as_bytes().as_ptr(), 32);
                    let _v_guard = CfGuard(val);

                    let add = make_cf_dict();
                    let _add_guard = CfGuard(add as CFTypeRef);
                    CFDictionarySetValue(add, kSecClass as _, kSecClassGenericPassword as _);
                    CFDictionarySetValue(add, kSecAttrService as _, service as _);
                    CFDictionarySetValue(add, kSecAttrAccount as _, account as _);
                    CFDictionarySetValue(add, kSecValueData as _, val as _);
                    if SecItemAdd(add, ptr::null_mut()) == ERR_SEC_SUCCESS {
                        return Ok(Some(mk));
                    }
                }
            }
            Ok(None)
        }
    }
}
