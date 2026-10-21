//! WeType's secp128r1 ECDH boundary. No Windows/SDK or OpenSSL runtime dependency.
//!
//! Domain parameters: SEC 2 version 1.0, section 2.3.1 (secp128r1, cofactor 1),
//! https://www.secg.org/SEC2-Ver-1.0.pdf . This legacy curve is required by the
//! observed peer protocol. The raw shared X coordinate is encoded as uppercase
//! hexadecimal ASCII and used directly as 32 AES key bytes, without a KDF.
//!
//! Scalar multiplication always executes 128 ladder rounds. Montgomery field
//! arithmetic, fixed-exponent inverses and conditional point selection avoid
//! secret-dependent exceptional-point branches. This is an implementation
//! property, not a claim of formal compiler or side-channel certification.
use crypto_bigint::{
    U128, impl_modulus,
    modular::{ConstMontyForm, ConstMontyParams},
};
use std::{
    fmt,
    sync::atomic::{Ordering, compiler_fence},
};
use subtle::{Choice, ConditionallySelectable, ConstantTimeEq, ConstantTimeLess};

impl_modulus!(Prime, U128, "FFFFFFFDFFFFFFFFFFFFFFFFFFFFFFFF");
type Field = ConstMontyForm<Prime, { Prime::LIMBS }>;
const P: U128 = U128::from_be_hex("FFFFFFFDFFFFFFFFFFFFFFFFFFFFFFFF");
const A: Field = Field::new(&U128::from_be_hex("FFFFFFFDFFFFFFFFFFFFFFFFFFFFFFFC"));
const B: Field = Field::new(&U128::from_be_hex("E87579C11079F43DD824993C2CEE5ED3"));
const ORDER: U128 = U128::from_be_hex("FFFFFFFE0000000075A30D1B9038A115");
const P_MINUS_TWO: U128 = U128::from_be_hex("FFFFFFFDFFFFFFFFFFFFFFFFFFFFFFFD");
const GX: Field = Field::new(&U128::from_be_hex("161FF7528B899B2D0C28607CA52C5B86"));
const GY: Field = Field::new(&U128::from_be_hex("CF5AC8395BAFEB13C02DA292DDED7A83"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Random,
    InvalidSecret,
    InvalidPublic,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Random => "ECDH random generation failed",
            Self::InvalidSecret => "invalid ECDH secret scalar",
            Self::InvalidPublic => "invalid secp128r1 public point",
        })
    }
}
impl std::error::Error for Error {}

/// No Clone/Copy, and Debug deliberately omits both private scalar and derived key.
pub struct SecretKey {
    scalar: [u8; 16],
    public: [u8; 33],
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretKey([REDACTED])")
    }
}

impl Drop for SecretKey {
    fn drop(&mut self) {
        erase(&mut self.scalar);
    }
}

fn erase(bytes: &mut [u8]) {
    // Best-effort erasure of our owned scalar; arithmetic/stack temporaries are
    // not covered by this guarantee and SecretKey is intentionally not Copy.
    for byte in bytes {
        unsafe {
            std::ptr::write_volatile(byte, 0);
        }
    }
    compiler_fence(Ordering::SeqCst);
}

impl SecretKey {
    pub fn generate() -> Result<Self, Error> {
        let mut scalar = [0; 16];
        // Uniform rejection sampling, rather than reduction modulo n. The cap
        // handles a broken entropy source without an unbounded loop.
        for _ in 0..128 {
            if getrandom::fill(&mut scalar).is_err() {
                erase(&mut scalar);
                return Err(Error::Random);
            }
            if valid_scalar(&scalar) {
                let result = Self::from_scalar(scalar);
                erase(&mut scalar);
                return result;
            }
        }
        erase(&mut scalar);
        Err(Error::Random)
    }

    fn from_scalar(mut scalar: [u8; 16]) -> Result<Self, Error> {
        if !valid_scalar(&scalar) {
            erase(&mut scalar);
            return Err(Error::InvalidSecret);
        }
        let point = Point::generator().multiply(&scalar);
        let public = point.encode();
        let key = Self { scalar, public };
        erase(&mut scalar);
        Ok(key)
    }

    /// SEC1 uncompressed raw point: 0x04 || X(16-byte BE) || Y(16-byte BE).
    /// The transport separately encodes these 33 bytes as 66 uppercase hex bytes.
    pub fn public_key(&self) -> [u8; 33] {
        self.public
    }

    /// Accept only a 33-byte SEC1 uncompressed raw point, not ASCII hex.
    /// Return the raw 16-byte ECDH X coordinate as 32 uppercase ASCII hex bytes.
    pub fn agree(&self, peer: &[u8]) -> Result<[u8; 32], Error> {
        let peer = Point::decode(peer)?;
        let shared = peer.multiply(&self.scalar);
        if bool::from(shared.infinity) {
            return Err(Error::InvalidPublic);
        }
        let mut x = shared.x.retrieve().to_be_bytes();
        let key = upper_hex(&x);
        erase(&mut x);
        Ok(key)
    }
}

fn valid_scalar(scalar: &[u8; 16]) -> bool {
    let value = U128::from_be_slice(scalar);
    bool::from(!value.ct_eq(&U128::ZERO) & value.ct_lt(&ORDER))
}

fn upper_hex(input: &[u8; 16]) -> [u8; 32] {
    // Arithmetic avoids a secret-indexed hex lookup table.
    let digit = |n: u8| b'0' + n + 7 * ((9_u16.wrapping_sub(u16::from(n)) >> 8) as u8 & 1);
    let mut output = [0; 32];
    for (index, &byte) in input.iter().enumerate() {
        output[index * 2] = digit(byte >> 4);
        output[index * 2 + 1] = digit(byte & 15);
    }
    output
}

#[derive(Clone, Copy)]
struct Point {
    x: Field,
    y: Field,
    infinity: Choice,
}

impl ConditionallySelectable for Point {
    fn conditional_select(a: &Self, b: &Self, choice: Choice) -> Self {
        Self {
            x: Field::conditional_select(&a.x, &b.x, choice),
            y: Field::conditional_select(&a.y, &b.y, choice),
            infinity: Choice::conditional_select(&a.infinity, &b.infinity, choice),
        }
    }
}

impl Point {
    fn identity() -> Self {
        Self {
            x: Field::ZERO,
            y: Field::ZERO,
            infinity: Choice::from(1),
        }
    }
    fn generator() -> Self {
        Self {
            x: GX,
            y: GY,
            infinity: Choice::from(0),
        }
    }

    fn decode(encoded: &[u8]) -> Result<Self, Error> {
        if encoded.len() != 33 || encoded[0] != 4 {
            return Err(Error::InvalidPublic);
        }
        let x = U128::from_be_slice(&encoded[1..17]);
        let y = U128::from_be_slice(&encoded[17..33]);
        // Reject non-canonical coordinates before Montgomery reduction.
        if !bool::from(x.ct_lt(&P) & y.ct_lt(&P)) {
            return Err(Error::InvalidPublic);
        }
        let (x, y) = (Field::new(&x), Field::new(&y));
        if !bool::from((y * y).ct_eq(&(x * x * x + A * x + B))) {
            return Err(Error::InvalidPublic);
        }
        // Cofactor h=1: every non-infinity point on this curve is in its
        // prime-order subgroup. SEC1 infinity and compressed forms are rejected.
        Ok(Self {
            x,
            y,
            infinity: Choice::from(0),
        })
    }

    fn encode(&self) -> [u8; 33] {
        let mut output = [0; 33];
        output[0] = 4;
        output[1..17].copy_from_slice(&self.x.retrieve().to_be_bytes());
        output[17..33].copy_from_slice(&self.y.retrieve().to_be_bytes());
        output
    }

    fn inverse(value: Field) -> Field {
        // p is prime. The public exponent always has 128 bits; zero maps to a
        // harmless zero candidate and exceptional points are selected below.
        value.pow(&P_MINUS_TWO)
    }

    fn double(&self) -> Self {
        let square = self.x * self.x;
        let slope = (square + square + square + A) * Self::inverse(self.y + self.y);
        let x = slope * slope - self.x - self.x;
        let candidate = Self {
            x,
            y: slope * (self.x - x) - self.y,
            infinity: Choice::from(0),
        };
        Self::conditional_select(
            &candidate,
            &Self::identity(),
            self.infinity | self.y.ct_eq(&Field::ZERO),
        )
    }

    fn add(&self, other: &Self) -> Self {
        let same_x = self.x.ct_eq(&other.x);
        let same_y = self.y.ct_eq(&other.y);
        let slope = (other.y - self.y) * Self::inverse(other.x - self.x);
        let x = slope * slope - self.x - other.x;
        let general = Self {
            x,
            y: slope * (self.x - x) - self.y,
            infinity: Choice::from(0),
        };
        let doubled = self.double();
        let mut result = Self::conditional_select(&general, &doubled, same_x & same_y);
        result = Self::conditional_select(&result, &Self::identity(), same_x & !same_y);
        result = Self::conditional_select(&result, other, self.infinity);
        Self::conditional_select(&result, self, other.infinity)
    }

    fn multiply(&self, scalar: &[u8; 16]) -> Self {
        let (mut zero, mut one) = (Self::identity(), *self);
        for &byte in scalar {
            for bit in (0..8).rev() {
                let choice = Choice::from((byte >> bit) & 1);
                let left = Self::conditional_select(&zero, &one, choice);
                let right = Self::conditional_select(&one, &zero, choice);
                let sum = left.add(&right);
                let double = left.double();
                zero = Self::conditional_select(&double, &sum, choice);
                one = Self::conditional_select(&sum, &double, choice);
            }
        }
        zero
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_hex<const N: usize>(text: &str) -> [u8; N] {
        assert_eq!(text.len(), N * 2);
        std::array::from_fn(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).unwrap())
    }

    #[test]
    fn sec2_generator_and_openssl_small_scalar_vectors() {
        for (scalar, public) in [
            (
                "00000000000000000000000000000001",
                "04161FF7528B899B2D0C28607CA52C5B86CF5AC8395BAFEB13C02DA292DDED7A83",
            ),
            (
                "00000000000000000000000000000002",
                "048151A0C6B92171DB199DB84BE753A97E03D853559455CAAE838395A9275B7E95",
            ),
            (
                "00000000000000000000000000000003",
                "040AD632F542942F23AA423B628A304B3B7AA67EE421C4E78851E4B4679BCDC41F",
            ),
            (
                "FFFFFFFE0000000075A30D1B9038A114",
                "04161FF7528B899B2D0C28607CA52C5B8630A537C4A45014EC3FD25D6D2212857C",
            ),
        ] {
            let key = SecretKey::from_scalar(decode_hex(scalar)).unwrap();
            assert_eq!(key.public_key(), decode_hex::<33>(public));
        }
    }

    #[test]
    fn agreement_is_symmetric_and_returns_hex_ascii_without_a_kdf() {
        let a = SecretKey::from_scalar(decode_hex("00000000000000000000000000000001")).unwrap();
        let b = SecretKey::from_scalar(decode_hex("00000000000000000000000000000002")).unwrap();
        let expected = *b"8151A0C6B92171DB199DB84BE753A97E";
        assert_eq!(a.agree(&b.public_key()).unwrap(), expected);
        assert_eq!(b.agree(&a.public_key()).unwrap(), expected);
        assert_eq!(
            upper_hex(&decode_hex("0001ABCDEF9876543210FEDCBA012345")),
            *b"0001ABCDEF9876543210FEDCBA012345"
        );
        assert_eq!(format!("{a:?}"), "SecretKey([REDACTED])");
    }

    #[test]
    fn exceptional_points_and_order_are_correct_without_special_case_branches() {
        let infinity = Point::identity();
        let g = Point::generator();
        let negative = Point {
            y: Field::ZERO - g.y,
            ..g
        };
        assert!(bool::from(g.add(&negative).infinity));
        assert!(bool::from(infinity.add(&infinity).infinity));
        assert_eq!(infinity.add(&g).encode(), g.encode());
        assert_eq!(g.add(&infinity).encode(), g.encode());
        assert_eq!(g.add(&g).encode(), g.double().encode());
        assert!(bool::from(g.multiply(&ORDER.to_be_bytes()).infinity));
        assert!(bool::from(g.multiply(&[0; 16]).infinity));
    }

    #[test]
    fn invalid_scalars_and_noncanonical_or_off_curve_peers_are_rejected() {
        assert!(matches!(
            SecretKey::from_scalar([0; 16]),
            Err(Error::InvalidSecret)
        ));
        assert!(matches!(
            SecretKey::from_scalar(ORDER.to_be_bytes()),
            Err(Error::InvalidSecret)
        ));
        assert!(matches!(
            SecretKey::from_scalar([0xff; 16]),
            Err(Error::InvalidSecret)
        ));
        let key = SecretKey::from_scalar(decode_hex("00000000000000000000000000000001")).unwrap();
        for bad in [
            vec![],
            vec![0],
            vec![4; 32],
            vec![4; 34],
            vec![0; 33],
            vec![4; 33],
        ] {
            assert_eq!(key.agree(&bad), Err(Error::InvalidPublic));
        }
        let mut point = key.public_key();
        point[0] = 2;
        assert_eq!(key.agree(&point), Err(Error::InvalidPublic));
        let mut point = key.public_key();
        point[1..17].copy_from_slice(&P.to_be_bytes());
        assert_eq!(key.agree(&point), Err(Error::InvalidPublic));
        let mut point = key.public_key();
        point[17..33].copy_from_slice(&P.to_be_bytes());
        assert_eq!(key.agree(&point), Err(Error::InvalidPublic));
        let mut point = key.public_key();
        point[32] ^= 1;
        assert_eq!(key.agree(&point), Err(Error::InvalidPublic));
    }

    #[test]
    fn generated_key_is_valid_and_owned_secret_bytes_can_be_erased() {
        let key = SecretKey::generate().unwrap();
        assert!(valid_scalar(&key.scalar));
        assert!(Point::decode(&key.public_key()).is_ok());
        let mut bytes = [0xab; 16];
        erase(&mut bytes);
        assert_eq!(bytes, [0; 16]);
    }

    #[test]
    #[cfg(unix)]
    #[ignore = "explicit Linux differential test; requires openssl secp128r1 support"]
    fn linux_openssl_public_keys_and_shared_secrets_match() {
        use std::{
            fs,
            io::Write,
            process::{Command, Stdio},
        };
        let root = std::env::temp_dir().join(format!(
            "wechat-ecdh-diff-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let peer_private = root.join("peer.pem");
        let status = Command::new("openssl")
            .args(["ecparam", "-name", "secp128r1", "-genkey", "-noout", "-out"])
            .arg(&peer_private)
            .status()
            .expect("Linux openssl required");
        assert!(status.success());
        let peer = Command::new("openssl")
            .args(["pkey", "-in"])
            .arg(&peer_private)
            .args(["-pubout", "-outform", "DER"])
            .output()
            .unwrap();
        assert!(peer.status.success());
        let peer_raw = &peer.stdout[peer.stdout.len() - 33..];
        let mut cases = vec![
            decode_hex("00000000000000000000000000000001"),
            decode_hex("00000000000000000000000000000002"),
            decode_hex("FFFFFFFE0000000075A30D1B9038A114"),
        ];
        for _ in 0..8 {
            let key = SecretKey::generate().unwrap();
            cases.push(key.scalar);
        }
        for scalar in cases {
            let key = SecretKey::from_scalar(scalar).unwrap();
            // SEC1 ECPrivateKey with explicit named-curve OID 1.3.132.0.28.
            let mut private = decode_hex::<7>("301E0201010410").to_vec();
            private.extend_from_slice(&scalar);
            private.extend_from_slice(&decode_hex::<9>("A00706052B8104001C"));
            let mut child = Command::new("openssl")
                .args(["pkey", "-inform", "DER", "-pubout", "-outform", "DER"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(&private).unwrap();
            erase(&mut private);
            let public = child.wait_with_output().unwrap();
            assert!(public.status.success());
            assert!(public.stdout.ends_with(&key.public_key()));
            let public_path = root.join("public.der");
            fs::write(&public_path, &public.stdout).unwrap();
            let shared = Command::new("openssl")
                .args(["pkeyutl", "-derive", "-inkey"])
                .arg(&peer_private)
                .args(["-peerkey"])
                .arg(&public_path)
                .args(["-peerform", "DER"])
                .output()
                .unwrap();
            assert!(shared.status.success());
            let raw: [u8; 16] = shared.stdout.try_into().unwrap();
            assert_eq!(key.agree(peer_raw).unwrap(), upper_hex(&raw));
        }
        fs::remove_file(peer_private).unwrap();
        fs::remove_file(root.join("public.der")).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
