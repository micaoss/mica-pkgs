//! SPAKE2-P256-SHA256-HKDF-HMAC (RFC 9382).

use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use p256::elliptic_curve::Field;
use p256::elliptic_curve::ff::PrimeField;
use p256::elliptic_curve::sec1::{FromSec1Point, ToSec1Point};
use p256::{AffinePoint, FieldBytes, ProjectivePoint, Scalar};
use sha2::{Digest, Sha256};

/// RFC 9382's M and N for P-256, compressed SEC1.
const M: [u8; 33] = hex33("02886e2f97ace46e55ba9dd7242579f2993b64e16ef3dcab95afd497333d8fa12f");
const N: [u8; 33] = hex33("03d8bbd6c639c62937b04d997f38c3770719c629d7014d49a24b4f98baa1292b49");

const fn hex33(s: &str) -> [u8; 33] {
    const fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            _ => c - b'a' + 10,
        }
    }
    let s = s.as_bytes();
    let mut out = [0; 33];
    let mut i = 0;
    while i < 33 {
        out[i] = (nibble(s[2 * i]) << 4) | nibble(s[2 * i + 1]);
        i += 1;
    }
    out
}

fn constant(bytes: &[u8; 33]) -> ProjectivePoint {
    // M and N are fixed valid points; the fallback is never taken.
    AffinePoint::from_sec1_bytes(bytes).map_or(ProjectivePoint::IDENTITY, ProjectivePoint::from)
}

/// A peer message that is not an uncompressed point on P-256.
#[derive(Debug, PartialEq, Eq)]
pub struct BadPoint;

impl std::fmt::Display for BadPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not an uncompressed P-256 point")
    }
}

impl std::error::Error for BadPoint {}

/// Decode a peer point: 65 bytes, `0x04`, coordinates below p, on the curve.
pub fn decode_point(bytes: &[u8]) -> Result<ProjectivePoint, BadPoint> {
    if bytes.len() != 65 || bytes[0] != 0x04 {
        return Err(BadPoint);
    }
    AffinePoint::from_sec1_bytes(bytes)
        .map(ProjectivePoint::from)
        .map_err(|_| BadPoint)
}

fn encode_point(point: &ProjectivePoint) -> Vec<u8> {
    point.to_affine().to_sec1_point(false).as_bytes().to_vec()
}

/// A uniformly random non-zero scalar.
pub fn random_scalar() -> Result<Scalar, getrandom::Error> {
    loop {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)?;
        if let Some(s) = Option::<Scalar>::from(Scalar::from_repr(FieldBytes::from(bytes)))
            && !bool::from(s.is_zero())
        {
            return Ok(s);
        }
    }
}

/// Which side of the exchange this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    A,
    B,
}

/// One side of a SPAKE2 exchange, after it has sent its share.
pub struct Spake2 {
    role: Role,
    id_a: Vec<u8>,
    id_b: Vec<u8>,
    w: Scalar,
    x: Scalar,
    message: Vec<u8>,
}

impl Spake2 {
    /// Start with the given ephemeral scalar (x for A, y for B).
    pub fn new(role: Role, id_a: &[u8], id_b: &[u8], w: Scalar, x: Scalar) -> Self {
        let own = constant(if role == Role::A { &M } else { &N });
        let message = encode_point(&(ProjectivePoint::GENERATOR * x + own * w));
        Self {
            role,
            id_a: id_a.to_vec(),
            id_b: id_b.to_vec(),
            w,
            x,
            message,
        }
    }

    /// pA or pB, uncompressed SEC1.
    pub fn message(&self) -> &[u8] {
        &self.message
    }

    /// Derive the keys from the peer's share.
    pub fn finish(self, peer: &[u8], aad: &[u8]) -> Result<Keys, BadPoint> {
        let peer_point = decode_point(peer)?;
        let other = constant(if self.role == Role::A { &N } else { &M });
        let k = (peer_point - other * self.w) * self.x;
        if k == ProjectivePoint::IDENTITY {
            return Err(BadPoint);
        }
        let (p_a, p_b) = match self.role {
            Role::A => (self.message.as_slice(), peer),
            Role::B => (peer, self.message.as_slice()),
        };
        let mut tt = Vec::new();
        for part in [
            &self.id_a[..],
            &self.id_b[..],
            p_a,
            p_b,
            &encode_point(&k),
            &self.w.to_repr()[..],
        ] {
            write_length_prefixed(&mut tt, part);
        }
        let hash = Sha256::digest(&tt);
        let mut ke = [0u8; 16];
        ke.copy_from_slice(&hash[..16]);
        let mut kc = [0u8; 32];
        let info = [b"ConfirmationKeys".as_slice(), aad].concat();
        Hkdf::<Sha256>::new(None, &hash[16..])
            .expand(&info, &mut kc)
            .map_err(|_| BadPoint)?;
        Ok(Keys {
            role: self.role,
            ke,
            kc,
            transcript: tt,
        })
    }
}

/// RFC 9382's `len(x) || x`, with an 8-byte little-endian length.
pub fn write_length_prefixed(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u64).to_le_bytes());
    out.extend_from_slice(data);
}

/// What a finished exchange yields.
pub struct Keys {
    role: Role,
    /// The shared secret Ke.
    pub ke: [u8; 16],
    kc: [u8; 32],
    transcript: Vec<u8>,
}

impl Keys {
    fn mac(&self, a: bool) -> Option<Hmac<Sha256>> {
        let key = if a { &self.kc[..16] } else { &self.kc[16..] };
        // HMAC takes a key of any length, so this is always Some.
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).ok()?;
        mac.update(&self.transcript);
        Some(mac)
    }

    /// cA for A, cB for B.
    pub fn confirmation(&self) -> Vec<u8> {
        self.mac(self.role == Role::A)
            .map(|mac| mac.finalize().into_bytes().to_vec())
            .unwrap_or_default()
    }

    /// Whether `peer` is the other side's confirmation, in constant time.
    pub fn verify(&self, peer: &[u8]) -> bool {
        self.mac(self.role == Role::B)
            .is_some_and(|mac| mac.verify_slice(peer).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn scalar(s: &str) -> Scalar {
        let bytes: [u8; 32] = hex(s).try_into().unwrap();
        Option::from(Scalar::from_repr(FieldBytes::from(bytes))).unwrap()
    }

    struct Vector {
        a: &'static str,
        b: &'static str,
        w: &'static str,
        x: &'static str,
        y: &'static str,
        p_a: &'static str,
        p_b: &'static str,
        ke: &'static str,
        conf_a: &'static str,
        conf_b: &'static str,
    }

    // RFC 9382 appendix B.
    const VECTORS: [Vector; 4] = [
        Vector {
            a: "server",
            b: "client",
            w: "2ee57912099d31560b3a44b1184b9b4866e904c49d12ac5042c97dca461b1a5f",
            x: "43dd0fd7215bdcb482879fca3220c6a968e66d70b1356cac18bb26c84a78d729",
            y: "dcb60106f276b02606d8ef0a328c02e4b629f84f89786af5befb0bc75b6e66be",
            p_a: "04a56fa807caaa53a4d28dbb9853b9815c61a411118a6fe516a8798434751470f9010153ac33d0d5f2047ffdb1a3e42c9b4e6be662766e1eeb4116988ede5f912c",
            p_b: "0406557e482bd03097ad0cbaa5df82115460d951e3451962f1eaf4367a420676d09857ccbc522686c83d1852abfa8ed6e4a1155cf8f1543ceca528afb591a1e0b7",
            ke: "0e0672dc86f8e45565d338b0540abe69",
            conf_a: "58ad4aa88e0b60d5061eb6b5dd93e80d9c4f00d127c65b3b35b1b5281fee38f0",
            conf_b: "d3e2e547f1ae04f2dbdbf0fc4b79f8ecff2dff314b5d32fe9fcef2fb26dc459b",
        },
        Vector {
            a: "",
            b: "client",
            w: "0548d8729f730589e579b0475a582c1608138ddf7054b73b5381c7e883e2efae",
            x: "403abbe3b1b4b9ba17e3032849759d723939a27a27b9d921c500edde18ed654b",
            y: "903023b6598908936ea7c929bd761af6039577a9c3f9581064187c3049d87065",
            p_a: "04a897b769e681c62ac1c2357319a3d363f610839c4477720d24cbe32f5fd85f44fb92ba966578c1b712be6962498834078262caa5b441ecfa9d4a9485720e918a",
            p_b: "04e0f816fd1c35e22065d5556215c097e799390d16661c386e0ecc84593974a61b881a8c82327687d0501862970c64565560cb5671f696048050ca66ca5f8cc7fc",
            ke: "642f05c473c2cd79909f9a841e2f30a7",
            conf_a: "47d29e6666af1b7dd450d571233085d7a9866e4d49d2645e2df975489521232b",
            conf_b: "3313c5cefc361d27fb16847a91c2a73b766ffa90a4839122a9b70a2f6bd1d6df",
        },
        Vector {
            a: "server",
            b: "",
            w: "626e0cdc7b14c9db3e52a0b1b3a768c98e37852d5db30febe0497b14eae8c254",
            x: "07adb3db6bc623d3399726bfdbfd3d15a58ea776ab8a308b00392621291f9633",
            y: "b6a4fc8dbb629d4ba51d6f91ed1532cf87adec98f25dd153a75accafafedec16",
            p_a: "04f88fb71c99bfffaea370966b7eb99cd4be0ff1a7d335caac4211c4afd855e2e15a873b298503ad8ba1d9cbb9a392d2ba309b48bfd7879aefd0f2cea6009763b0",
            p_b: "040c269d6be017dccb15182ac6bfcd9e2a14de019dd587eaf4bdfd353f031101e7cca177f8eb362a6e83e7d5e729c0732e1b528879c086f39ba0f31a9661bd34db",
            ke: "005184ff460da2ce59062c87733c299c",
            conf_a: "bc9f9bbe99f26d0b2260e6456e05a86196a3307ec6663a18bf6ac825736533b2",
            conf_b: "c2370e1bf813b086dff0d834e74425a06e6390f48f5411900276dcccc5a297ec",
        },
        Vector {
            a: "",
            b: "",
            w: "7bf46c454b4c1b25799527d896508afd5fc62ef4ec59db1efb49113063d70cca",
            x: "8cef65df64bb2d0f83540c53632de911b5b24b3eab6cc74a97609fd659e95473",
            y: "d7a66f64074a84652d8d623a92e20c9675c61cb5b4f6a0063e4648a2fdc02d53",
            p_a: "04a65b367a3f613cf9f0654b1b28a1e3a8a40387956c8ba6063e8658563890f46ca1ef6a676598889fc28de2950ab8120b79a5ef1ea4c9f44bc98f585634b46d66",
            p_b: "04589f13218822710d98d8b2123a079041052d9941b9cf88c6617ddb2fcc0494662eea8ba6b64692dc318250030c6af045cb738bc81ba35b043c3dcb46adf6f58d",
            ke: "fc6374762ba5cf11f4b2caa08b2cd1b9",
            conf_a: "dfb4db8d48ae5a675963ea5e6c19d98d4ea028d8e898dad96ea19a80ade95dca",
            conf_b: "d0f0609d1613138d354f7e95f19fb556bf52d751947241e8c7118df5ef0ae175",
        },
    ];

    #[test]
    fn rfc_9382_appendix_b() {
        for v in &VECTORS {
            let w = scalar(v.w);
            let a = Spake2::new(Role::A, v.a.as_bytes(), v.b.as_bytes(), w, scalar(v.x));
            let b = Spake2::new(Role::B, v.a.as_bytes(), v.b.as_bytes(), w, scalar(v.y));
            assert_eq!(a.message(), hex(v.p_a));
            assert_eq!(b.message(), hex(v.p_b));
            let (pa, pb) = (a.message().to_vec(), b.message().to_vec());
            let ka = a.finish(&pb, b"").unwrap();
            let kb = b.finish(&pa, b"").unwrap();
            assert_eq!(ka.ke.to_vec(), hex(v.ke));
            assert_eq!(kb.ke.to_vec(), hex(v.ke));
            assert_eq!(ka.confirmation(), hex(v.conf_a));
            assert_eq!(kb.confirmation(), hex(v.conf_b));
            assert!(kb.verify(&ka.confirmation()));
            assert!(ka.verify(&kb.confirmation()));
            assert!(!kb.verify(&kb.confirmation()));
        }
    }

    #[test]
    fn different_passwords_do_not_confirm() {
        let a = Spake2::new(
            Role::A,
            b"pin",
            b"d",
            Scalar::from(1u64),
            random_scalar().unwrap(),
        );
        let b = Spake2::new(
            Role::B,
            b"pin",
            b"d",
            Scalar::from(2u64),
            random_scalar().unwrap(),
        );
        let (pa, pb) = (a.message().to_vec(), b.message().to_vec());
        let ka = a.finish(&pb, b"x").unwrap();
        let kb = b.finish(&pa, b"x").unwrap();
        assert_ne!(ka.ke, kb.ke);
        assert!(!kb.verify(&ka.confirmation()));
    }

    #[test]
    fn malformed_points_are_refused() {
        let good = hex(VECTORS[0].p_a);
        assert!(decode_point(&good).is_ok());
        assert_eq!(decode_point(&good[..64]), Err(BadPoint));
        let mut prefix = good.clone();
        prefix[0] = 0x02;
        assert_eq!(decode_point(&prefix), Err(BadPoint));
        let mut off = good.clone();
        off[64] ^= 1;
        assert_eq!(decode_point(&off), Err(BadPoint));
        let mut big = vec![0x04];
        big.extend([0xff; 64]);
        assert_eq!(decode_point(&big), Err(BadPoint));
        assert_eq!(decode_point(&M), Err(BadPoint));
    }
}
