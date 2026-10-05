use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{fs::OpenOptions, io::Write, path::Path};
use zeroize::Zeroizing;

#[derive(Serialize, Deserialize)]
struct KeyEnvelope {
    version: u8,
    salt: Vec<u8>,
    wrapped: Vec<u8>,
}

pub struct Vault {
    key: Zeroizing<[u8; 32]>,
}

fn random<const N: usize>() -> anyhow::Result<[u8; N]> {
    let mut value = [0; N];
    getrandom::fill(&mut value).map_err(|_| anyhow::anyhow!("random_failed"))?;
    Ok(value)
}

fn derive(password: &str, salt: &[u8]) -> anyhow::Result<Zeroizing<[u8; 32]>> {
    let mut key = Zeroizing::new([0u8; 32]);
    let params =
        Params::new(32768, 3, 1, Some(32)).map_err(|_| anyhow::anyhow!("key_derivation_failed"))?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, key.as_mut())
        .map_err(|_| anyhow::anyhow!("key_derivation_failed"))?;
    Ok(key)
}

impl Vault {
    pub fn open(path: &Path, password: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(!password.is_empty(), "vault_password_required");
        if path.exists() {
            let file = std::fs::read(path)?;
            anyhow::ensure!(file.len() <= 4096, "invalid_key_file");
            let env: KeyEnvelope = serde_json::from_slice(&file)?;
            anyhow::ensure!(env.version == 1 && env.salt.len() == 16, "invalid_key_file");
            let kek = Self {
                key: derive(password, &env.salt)?,
            };
            let clear = kek.decrypt("master-key:v1", &env.wrapped)?;
            anyhow::ensure!(clear.len() == 32, "invalid_key_file");
            let mut key = Zeroizing::new([0; 32]);
            key.copy_from_slice(&clear);
            Ok(Self { key })
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let salt = random::<16>()?;
            let kek = Self {
                key: derive(password, &salt)?,
            };
            let key = Zeroizing::new(random::<32>()?);
            let env = KeyEnvelope {
                version: 1,
                salt: salt.to_vec(),
                wrapped: kek.encrypt("master-key:v1", key.as_ref())?,
            };
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(path)?;
            file.write_all(&serde_json::to_vec(&env)?)?;
            file.sync_all()?;
            Ok(Self { key })
        }
    }

    pub fn encrypt(&self, context: &str, clear: &[u8]) -> anyhow::Result<Vec<u8>> {
        let nonce = random::<24>()?;
        let cipher = XChaCha20Poly1305::new_from_slice(self.key.as_ref())
            .map_err(|_| anyhow::anyhow!("encrypt_failed"))?;
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: clear,
                    aad: context.as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!("encrypt_failed"))?;
        let mut result = Vec::with_capacity(24 + ciphertext.len());
        result.extend_from_slice(&nonce);
        result.extend(ciphertext);
        Ok(result)
    }

    pub fn decrypt(&self, context: &str, blob: &[u8]) -> anyhow::Result<Zeroizing<Vec<u8>>> {
        anyhow::ensure!(blob.len() >= 40, "invalid_password");
        let cipher = XChaCha20Poly1305::new_from_slice(self.key.as_ref())
            .map_err(|_| anyhow::anyhow!("decrypt_failed"))?;
        cipher
            .decrypt(
                XNonce::from_slice(&blob[..24]),
                Payload {
                    msg: &blob[24..],
                    aad: context.as_bytes(),
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| anyhow::anyhow!("invalid_password"))
    }

    pub fn index(&self, context: &str, text: &str) -> String {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(self.key.as_ref())
            .expect("HMAC accepts all key sizes");
        mac.update(context.as_bytes());
        mac.update(&[0]);
        mac.update(text.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

pub fn decode_legacy(blob: &[u8], password: &str) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    anyhow::ensure!(
        blob.len() >= 67 && &blob[..3] == b"HV1",
        "invalid_legacy_vault"
    );
    let mut key = Zeroizing::new([0u8; 32]);
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), &blob[3..19], 100_000, key.as_mut());
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key.as_ref()).unwrap();
    mac.update(&blob[19..35]);
    mac.update(&blob[67..]);
    mac.verify_slice(&blob[35..67])
        .map_err(|_| anyhow::anyhow!("invalid_password"))?;
    use sha2::Digest;
    let mut result = Zeroizing::new(Vec::with_capacity(blob.len() - 67));
    for (counter, chunk) in blob[67..].chunks(32).enumerate() {
        let mut hasher = Sha256::new();
        hasher.update(key.as_ref());
        hasher.update(&blob[19..35]);
        hasher.update((counter as u64).to_be_bytes());
        let stream = hasher.finalize();
        result.extend(chunk.iter().zip(stream).map(|(a, b)| a ^ b));
    }
    Ok(result)
}
