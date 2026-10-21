//! Cipher framing proven at the WeType AES export and captured wire boundary.
use crate::Result;
use aes::{
    Aes256,
    cipher::{BlockDecrypt, BlockEncrypt, KeyInit},
};
use subtle::ConstantTimeEq;

pub fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
    let cipher = Aes256::new(key.into());
    let padding = 16 - plaintext.len() % 16;
    let mut result = Vec::with_capacity(plaintext.len() + padding);
    result.extend_from_slice(plaintext);
    result.resize(plaintext.len() + padding, padding as u8);
    for block in result.chunks_exact_mut(16) {
        cipher.encrypt_block(block.into());
    }
    result
}

pub fn decrypt(key: &[u8; 32], ciphertext: &[u8]) -> Result<Vec<u8>> {
    if ciphertext.is_empty() || ciphertext.len() % 16 != 0 || ciphertext.len() > (1 << 20) {
        return Err("invalid WeType ciphertext size".into());
    }
    let cipher = Aes256::new(key.into());
    let mut result = ciphertext.to_vec();
    for block in result.chunks_exact_mut(16) {
        cipher.decrypt_block(block.into());
    }
    let padding = result[result.len() - 1] as usize;
    let mut valid = (1..=16).contains(&padding) as u8;
    let start = result.len() - 16;
    // Inspect the full last block before reporting any padding failure.
    for i in 0..16 {
        let required = (i >= 16_usize.saturating_sub(padding)) as u8;
        valid &= (1 - required) | result[start + i].ct_eq(&(padding as u8)).unwrap_u8();
    }
    if valid != 1 {
        return Err("invalid WeType encrypted response".into());
    }
    result.truncate(result.len() - padding);
    Ok(result)
}

pub fn hex_upper(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut result = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        result.push(DIGITS[(b >> 4) as usize] as char);
        result.push(DIGITS[(b & 15) as usize] as char);
    }
    result
}

pub fn unhex(text: &str) -> Result<Vec<u8>> {
    if text.len() % 2 != 0 || text.len() > 4096 {
        return Err("invalid WeType hexadecimal value".into());
    }
    fn nibble(b: u8) -> Result<u8> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            b'A'..=b'F' => Ok(b - b'A' + 10),
            _ => Err("invalid WeType hexadecimal value".into()),
        }
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|p| Ok(nibble(p[0])? << 4 | nibble(p[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes256_matches_nist_first_block_and_rejects_padding_damage() {
        let key: [u8; 32] =
            unhex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4")
                .unwrap()
                .try_into()
                .unwrap();
        let input = unhex("6bc1bee22e409f96e93d7e117393172a").unwrap();
        let mut encrypted = encrypt(&key, &input);
        assert_eq!(
            hex_upper(&encrypted[..16]),
            "F3EED1BDB5D2A03C064B5A7E3DB181F8"
        );
        assert_eq!(decrypt(&key, &encrypted).unwrap(), input);
        // Copy the known plaintext block over the padding block: decryption is
        // deterministic and must fail because its last byte is not PKCS#7.
        let first: [u8; 16] = encrypted[..16].try_into().unwrap();
        encrypted[16..].copy_from_slice(&first);
        assert!(decrypt(&key, &encrypted).is_err());
        assert!(decrypt(&key, &[]).is_err());
    }
}
