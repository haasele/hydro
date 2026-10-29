//! `hashlib` and the C accelerator surfaces: `_hashlib`, `_sha1`, `_sha2`,
//! `_md5`, `_blake2`, `_hmac` and the `_uuid` time generator.
//!
//! The digest implementations are self-contained Rust (no new dependencies):
//! MD5 (RFC 1321), SHA-1 (FIPS 180-4), SHA-224/256/384/512 (FIPS 180-4),
//! BLAKE2b/BLAKE2s (RFC 7693, including the keyed and tree parameters),
//! HMAC (RFC 2104) and PBKDF2-HMAC (RFC 8018). Every hash object keeps its
//! running state in a hidden instance attribute, so `update`, `copy`,
//! `digest` and `hexdigest` are incremental and independent.
//!
//! Algorithms that need SHA-3/SHAKE are not implemented; `new`,
//! `algorithms_guaranteed`/`algorithms_available` therefore report only the
//! algorithms that really exist and never fabricate a digest. The `uuid1`,
//! `uuid4` and `uuid5` helpers return canonical UUID strings, matching
//! `str(uuid.uuidN(...))`; `uuid5` hashes with the native SHA-1. Only
//! `_uuid.generate_time_safe()` returns the raw 16-byte form its caller
//! (Lib/uuid.py) feeds to `UUID(bytes=...)`.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use hydro_ir::MethodKind;

use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, BLOCKING_IO_ERROR, OS_ERROR, OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::methods;
use crate::ops;
use crate::value::{self, NONE};

const MODULE: &str = "hashlib";
const HASH_CLASS_KEY: &str = "hashlib.HASH.class";
const HMAC_CLASS_KEY: &str = "hashlib.HMAC.class";

/// Hidden instance attributes. The leading NUL keeps them out of the normal
/// attribute namespace, like the payload attributes of other native classes.
const STATE_ATTR: &str = "\u{0}hashlib.state";
const INNER_ATTR: &str = "\u{0}hashlib.inner";
const OUTER_ATTR: &str = "\u{0}hashlib.outer";

/// The methods every hash object exposes. `arity` includes `self`.
///
/// The native name is bare (`update`, ...), exactly like the other native
/// class tables: `methods.rs` prepends the receiver, `resolve_native` matches
/// `hashlib.update` in the supplemental table, and the receiver decides
/// whether the HASH or HMAC implementation runs.
const HASH_METHODS: [(&str, &str, usize); 4] = [
    ("update", "update", 2),
    ("digest", "digest", 1),
    ("hexdigest", "hexdigest", 1),
    ("copy", "copy", 1),
];

/// The methods every HMAC object exposes.
const HMAC_METHODS: [(&str, &str, usize); 4] = [
    ("update", "update", 2),
    ("digest", "digest", 1),
    ("hexdigest", "hexdigest", 1),
    ("copy", "copy", 1),
];

const MAX_DIGEST_BYTES: usize = 64;
const MAX_BLOCK_BYTES: usize = 128;

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn overflow_error(message: &str) -> i64 {
    exc::raise_with_message(OVERFLOW_ERROR, message)
}

fn attribute_error(module: &str, name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module '{module}' has no attribute '{name}'"),
    )
}

// -------------------------------------------------------------------------
// Algorithms
// -------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Algo {
    Md5,
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
    Blake2b,
    Blake2s,
}

impl Algo {
    fn from_name(name: &str) -> Option<Algo> {
        // CPython accepts the OpenSSL spellings for the SHA-2 family. The
        // BLAKE2 constructors are lower-case only, which `new` enforces
        // separately.
        match name.to_ascii_lowercase().as_str() {
            "md5" => Some(Algo::Md5),
            "sha1" | "sha-1" => Some(Algo::Sha1),
            "sha224" | "sha-224" | "sha2-224" => Some(Algo::Sha224),
            "sha256" | "sha-256" | "sha2-256" => Some(Algo::Sha256),
            "sha384" | "sha-384" | "sha2-384" => Some(Algo::Sha384),
            "sha512" | "sha-512" | "sha2-512" => Some(Algo::Sha512),
            "blake2b" => Some(Algo::Blake2b),
            "blake2s" => Some(Algo::Blake2s),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Algo::Md5 => "md5",
            Algo::Sha1 => "sha1",
            Algo::Sha224 => "sha224",
            Algo::Sha256 => "sha256",
            Algo::Sha384 => "sha384",
            Algo::Sha512 => "sha512",
            Algo::Blake2b => "blake2b",
            Algo::Blake2s => "blake2s",
        }
    }

    fn digest_size(self) -> usize {
        match self {
            Algo::Md5 => 16,
            Algo::Sha1 => 20,
            Algo::Sha224 => 28,
            Algo::Sha256 => 32,
            Algo::Sha384 => 48,
            Algo::Sha512 => 64,
            Algo::Blake2b => 64,
            Algo::Blake2s => 32,
        }
    }

    fn block_size(self) -> usize {
        match self {
            Algo::Md5 | Algo::Sha1 | Algo::Sha224 | Algo::Sha256 => 64,
            Algo::Sha384 | Algo::Sha512 | Algo::Blake2b => 128,
            Algo::Blake2s => 64,
        }
    }

    fn is_blake2(self) -> bool {
        matches!(self, Algo::Blake2b | Algo::Blake2s)
    }

    fn tag(self) -> u8 {
        match self {
            Algo::Md5 => 0,
            Algo::Sha1 => 1,
            Algo::Sha224 => 2,
            Algo::Sha256 => 3,
            Algo::Sha384 => 4,
            Algo::Sha512 => 5,
            Algo::Blake2b => 6,
            Algo::Blake2s => 7,
        }
    }

    fn from_tag(tag: u8) -> Option<Algo> {
        Some(match tag {
            0 => Algo::Md5,
            1 => Algo::Sha1,
            2 => Algo::Sha224,
            3 => Algo::Sha256,
            4 => Algo::Sha384,
            5 => Algo::Sha512,
            6 => Algo::Blake2b,
            7 => Algo::Blake2s,
            _ => return None,
        })
    }
}

// -------------------------------------------------------------------------
// Running digest state
// -------------------------------------------------------------------------

/// One incremental digest. The state is a fixed set of words plus a pending
/// block; `serialize`/`deserialize` round-trip it through a hidden
/// `bytearray`, which keeps `copy()` cheap and independent.
#[derive(Clone, Debug)]
struct DigestState {
    algo: Algo,
    /// Digest state words (32-bit algorithms keep them zero-extended).
    h: [u64; 8],
    /// Number of live words in `h`.
    hlen: usize,
    /// Total number of bytes already compressed.
    len: u128,
    /// Bytes not yet compressed; always shorter than a block, except that a
    /// BLAKE2 state may hold one full final block.
    buf: Vec<u8>,
    outlen: usize,
    last_node: bool,
}

const MD5_IV: [u64; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
const SHA1_IV: [u64; 5] = [
    0x6745_2301,
    0xefcd_ab89,
    0x98ba_dcfe,
    0x1032_5476,
    0xc3d2_e1f0,
];
const SHA224_IV: [u64; 8] = [
    0xc105_9ed8,
    0x367c_d507,
    0x3070_dd17,
    0xf70e_5939,
    0xffc0_0b31,
    0x6858_1511,
    0x64f9_8fa7,
    0xbefa_4fa4,
];
const SHA256_IV: [u64; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];
const SHA384_IV: [u64; 8] = [
    0xcbbb_9d5d_c105_9ed8,
    0x629a_292a_367c_d507,
    0x9159_015a_3070_dd17,
    0x152f_ecd8_f70e_5939,
    0x6733_2667_ffc0_0b31,
    0x8eb4_4a87_6858_1511,
    0xdb0c_2e0d_64f9_8fa7,
    0x47b5_481d_befa_4fa4,
];
const SHA512_IV: [u64; 8] = [
    0x6a09_e667_f3bc_c908,
    0xbb67_ae85_84ca_a73b,
    0x3c6e_f372_fe94_f82b,
    0xa54f_f53a_5f1d_36f1,
    0x510e_527f_ade6_82d1,
    0x9b05_688c_2b3e_6c1f,
    0x1f83_d9ab_fb41_bd6b,
    0x5be0_cd19_137e_2179,
];
const BLAKE2B_IV: [u64; 8] = SHA512_IV;
const BLAKE2S_IV: [u64; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

impl DigestState {
    /// A default state for `algo` (`blake2b`/`blake2s` use the standard
    /// 64/32-byte digest and an empty key).
    fn new(algo: Algo) -> DigestState {
        match algo {
            Algo::Blake2b | Algo::Blake2s => DigestState::new_blake2(
                algo,
                algo.digest_size(),
                &[],
                &[],
                &[],
                1,
                1,
                0,
                0,
                0,
                0,
                false,
            ),
            _ => DigestState::new_plain(algo),
        }
    }

    fn new_plain(algo: Algo) -> DigestState {
        let (words, outlen): (&[u64], usize) = match algo {
            Algo::Md5 => (&MD5_IV, 16),
            Algo::Sha1 => (&SHA1_IV, 20),
            Algo::Sha224 => (&SHA224_IV, 28),
            Algo::Sha256 => (&SHA256_IV, 32),
            Algo::Sha384 => (&SHA384_IV, 48),
            Algo::Sha512 => (&SHA512_IV, 64),
            Algo::Blake2b | Algo::Blake2s => {
                return DigestState::new(algo);
            }
        };
        let mut h = [0u64; 8];
        h[..words.len()].copy_from_slice(words);
        DigestState {
            algo,
            h,
            hlen: words.len(),
            len: 0,
            buf: Vec::new(),
            outlen,
            last_node: false,
        }
    }

    /// Builds a BLAKE2 state from already validated parameters.
    #[allow(clippy::too_many_arguments)]
    fn new_blake2(
        algo: Algo,
        outlen: usize,
        key: &[u8],
        salt: &[u8],
        person: &[u8],
        fanout: u8,
        depth: u8,
        leaf_size: u32,
        node_offset: u64,
        node_depth: u8,
        inner_size: u8,
        last_node: bool,
    ) -> DigestState {
        let blake_b = algo == Algo::Blake2b;
        let block = algo.block_size();
        let mut h = if blake_b { BLAKE2B_IV } else { BLAKE2S_IV };
        if blake_b {
            let mut p = [0u8; 64];
            p[0] = outlen as u8;
            p[1] = key.len() as u8;
            p[2] = fanout;
            p[3] = depth;
            p[4..8].copy_from_slice(&leaf_size.to_le_bytes());
            p[8..16].copy_from_slice(&node_offset.to_le_bytes());
            p[16] = node_depth;
            p[17] = inner_size;
            p[32..32 + salt.len()].copy_from_slice(salt);
            p[48..48 + person.len()].copy_from_slice(person);
            for (index, word) in h.iter_mut().enumerate() {
                *word ^= u64_at(&p, index * 8);
            }
        } else {
            let mut p = [0u8; 32];
            p[0] = outlen as u8;
            p[1] = key.len() as u8;
            p[2] = fanout;
            p[3] = depth;
            p[4..8].copy_from_slice(&leaf_size.to_le_bytes());
            p[8..14].copy_from_slice(&node_offset.to_le_bytes()[..6]);
            p[14] = node_depth;
            p[15] = inner_size;
            p[16..16 + salt.len()].copy_from_slice(salt);
            p[24..24 + person.len()].copy_from_slice(person);
            for (index, word) in h.iter_mut().enumerate() {
                *word ^= u64::from(u32_at(&p, index * 4));
            }
        }
        let mut buf = Vec::new();
        if !key.is_empty() {
            buf.extend_from_slice(key);
            buf.resize(block, 0);
        }
        DigestState {
            algo,
            h,
            hlen: 8,
            len: 0,
            buf,
            outlen,
            last_node,
        }
    }

    fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24 + self.hlen * 8 + self.buf.len());
        out.push(self.algo.tag());
        out.push(self.outlen as u8);
        out.push(u8::from(self.last_node));
        out.extend_from_slice(&self.len.to_le_bytes());
        out.push(self.hlen as u8);
        for word in &self.h[..self.hlen] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.push(self.buf.len() as u8);
        out.extend_from_slice(&self.buf);
        out
    }

    fn deserialize(bytes: &[u8]) -> Option<DigestState> {
        let mut at = 0usize;
        let algo = Algo::from_tag(*bytes.get(at)?)?;
        at += 1;
        let outlen = usize::from(*bytes.get(at)?);
        at += 1;
        let last_node = *bytes.get(at)? != 0;
        at += 1;
        let len = u128::from_le_bytes(bytes.get(at..at + 16)?.try_into().ok()?);
        at += 16;
        let hlen = usize::from(*bytes.get(at)?);
        at += 1;
        if hlen == 0 || hlen > 8 || outlen == 0 || outlen > MAX_DIGEST_BYTES {
            return None;
        }
        let mut h = [0u64; 8];
        for word in h.iter_mut().take(hlen) {
            *word = u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?);
            at += 8;
        }
        let buf_len = usize::from(*bytes.get(at)?);
        at += 1;
        if buf_len > MAX_BLOCK_BYTES {
            return None;
        }
        let buf = bytes.get(at..at + buf_len)?.to_vec();
        at += buf_len;
        if at != bytes.len() {
            return None;
        }
        Some(DigestState {
            algo,
            h,
            hlen,
            len,
            buf,
            outlen,
            last_node,
        })
    }

    fn update(&mut self, data: &[u8]) {
        let block = self.algo.block_size();
        let blake = self.algo.is_blake2();
        let DigestState {
            algo,
            h,
            len,
            buf,
            last_node,
            ..
        } = self;
        let mut data = data;
        if !buf.is_empty() {
            let take = (block - buf.len()).min(data.len());
            buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if buf.len() == block && (blake && !data.is_empty() || !blake) {
                let offset = if blake { *len + block as u128 } else { 0 };
                compress(*algo, h, &buf[..block], offset, false, *last_node);
                *len += block as u128;
                buf.clear();
            }
        }
        if blake {
            // Keep one full block in the buffer so the final compression can
            // carry the finalization flag.
            while data.len() > block {
                compress(
                    *algo,
                    h,
                    &data[..block],
                    *len + block as u128,
                    false,
                    *last_node,
                );
                *len += block as u128;
                data = &data[block..];
            }
        } else {
            while data.len() >= block {
                compress(*algo, h, &data[..block], 0, false, false);
                *len += block as u128;
                data = &data[block..];
            }
        }
        buf.extend_from_slice(data);
    }

    fn finalize(&self) -> Vec<u8> {
        let mut copy = self.clone();
        let block = copy.algo.block_size();
        if copy.algo.is_blake2() {
            let total = copy.len + copy.buf.len() as u128;
            copy.buf.resize(block, 0);
            let DigestState {
                algo,
                h,
                buf,
                last_node,
                outlen,
                ..
            } = &mut copy;
            compress(*algo, h, &buf[..block], total, true, *last_node);
            let mut out = Vec::with_capacity(*outlen);
            if *algo == Algo::Blake2s {
                // BLAKE2s words are 32 bits; widening them to u64 would emit
                // four zero bytes after every word.
                for index in 0..(*outlen).div_ceil(4) {
                    out.extend_from_slice(&(h[index.min(7)] as u32).to_le_bytes());
                }
            } else {
                for index in 0..(*outlen).div_ceil(8) {
                    out.extend_from_slice(&h[index.min(7)].to_le_bytes());
                }
            }
            out.truncate(*outlen);
            return out;
        }
        let total = copy.len + copy.buf.len() as u128;
        let bit_len = total.wrapping_mul(8);
        copy.buf.push(0x80);
        let length_bytes = if copy.algo == Algo::Sha384 || copy.algo == Algo::Sha512 {
            16
        } else {
            8
        };
        while copy.buf.len() % block != block - length_bytes {
            copy.buf.push(0);
        }
        match copy.algo {
            Algo::Md5 => copy.buf.extend_from_slice(&(bit_len as u64).to_le_bytes()),
            Algo::Sha1 | Algo::Sha224 | Algo::Sha256 => {
                copy.buf.extend_from_slice(&(bit_len as u64).to_be_bytes());
            }
            Algo::Sha384 | Algo::Sha512 => copy.buf.extend_from_slice(&bit_len.to_be_bytes()),
            Algo::Blake2b | Algo::Blake2s => unreachable!(),
        }
        {
            let DigestState { algo, h, buf, .. } = &mut copy;
            let mut start = 0;
            while start < buf.len() {
                compress(*algo, h, &buf[start..start + block], 0, false, false);
                start += block;
            }
        }
        let mut out = Vec::with_capacity(copy.outlen);
        match copy.algo {
            Algo::Md5 => {
                for word in &copy.h[..4] {
                    out.extend_from_slice(&(*word as u32).to_le_bytes());
                }
            }
            Algo::Sha1 => {
                for word in &copy.h[..5] {
                    out.extend_from_slice(&(*word as u32).to_be_bytes());
                }
            }
            Algo::Sha224 => {
                for word in &copy.h[..7] {
                    out.extend_from_slice(&(*word as u32).to_be_bytes());
                }
            }
            Algo::Sha256 => {
                for word in &copy.h[..8] {
                    out.extend_from_slice(&(*word as u32).to_be_bytes());
                }
            }
            Algo::Sha384 => {
                for word in &copy.h[..6] {
                    out.extend_from_slice(&word.to_be_bytes());
                }
            }
            Algo::Sha512 => {
                for word in &copy.h[..8] {
                    out.extend_from_slice(&word.to_be_bytes());
                }
            }
            Algo::Blake2b | Algo::Blake2s => unreachable!(),
        }
        out.truncate(copy.outlen);
        out
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes([
        bytes[at],
        bytes[at + 1],
        bytes[at + 2],
        bytes[at + 3],
        bytes[at + 4],
        bytes[at + 5],
        bytes[at + 6],
        bytes[at + 7],
    ])
}

/// Dispatches one block to the algorithm's compression function.
fn compress(algo: Algo, h: &mut [u64; 8], block: &[u8], t: u128, last: bool, last_node: bool) {
    match algo {
        Algo::Md5 => md5_compress(h, block),
        Algo::Sha1 => sha1_compress(h, block),
        Algo::Sha224 | Algo::Sha256 => sha256_compress(h, block),
        Algo::Sha384 | Algo::Sha512 => sha512_compress(h, block),
        Algo::Blake2b => blake2b_compress(h, block, t, last, last_node),
        Algo::Blake2s => blake2s_compress(h, block, t, last, last_node),
    }
}

// -------------------------------------------------------------------------
// MD5
// -------------------------------------------------------------------------

const MD5_SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const MD5_K: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

fn md5_compress(h: &mut [u64; 8], block: &[u8]) {
    let mut w = [0u32; 16];
    for (index, word) in w.iter_mut().enumerate() {
        *word = u32_at(block, index * 4);
    }
    let (mut a, mut b, mut c, mut d) = (h[0] as u32, h[1] as u32, h[2] as u32, h[3] as u32);
    for i in 0..64 {
        let (f, g) = match i / 16 {
            0 => ((b & c) | (!b & d), i),
            1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
            2 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | !d), (7 * i) % 16),
        };
        let f = f.wrapping_add(a).wrapping_add(MD5_K[i]).wrapping_add(w[g]);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(f.rotate_left(MD5_SHIFTS[i]));
    }
    h[0] = h[0].wrapping_add(a as u64);
    h[1] = h[1].wrapping_add(b as u64);
    h[2] = h[2].wrapping_add(c as u64);
    h[3] = h[3].wrapping_add(d as u64);
}

// -------------------------------------------------------------------------
// SHA-1
// -------------------------------------------------------------------------

fn sha1_compress(h: &mut [u64; 8], block: &[u8]) {
    let mut w = [0u32; 80];
    for (index, word) in w.iter_mut().take(16).enumerate() {
        *word = u32::from_be_bytes([
            block[index * 4],
            block[index * 4 + 1],
            block[index * 4 + 2],
            block[index * 4 + 3],
        ]);
    }
    for index in 16..80 {
        w[index] = (w[index - 3] ^ w[index - 8] ^ w[index - 14] ^ w[index - 16]).rotate_left(1);
    }
    let (mut a, mut b, mut c, mut d, mut e) = (
        h[0] as u32,
        h[1] as u32,
        h[2] as u32,
        h[3] as u32,
        h[4] as u32,
    );
    for (index, word) in w.iter().enumerate() {
        let (f, k) = match index / 20 {
            0 => ((b & c) | (!b & d), 0x5a82_7999),
            1 => (b ^ c ^ d, 0x6ed9_eba1),
            2 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
            _ => (b ^ c ^ d, 0xca62_c1d6),
        };
        let temp = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(*word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = temp;
    }
    h[0] = h[0].wrapping_add(a as u64);
    h[1] = h[1].wrapping_add(b as u64);
    h[2] = h[2].wrapping_add(c as u64);
    h[3] = h[3].wrapping_add(d as u64);
    h[4] = h[4].wrapping_add(e as u64);
}

// -------------------------------------------------------------------------
// SHA-256 / SHA-224
// -------------------------------------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

fn sha256_compress(h: &mut [u64; 8], block: &[u8]) {
    let mut w = [0u32; 64];
    for (index, word) in w.iter_mut().take(16).enumerate() {
        *word = u32::from_be_bytes([
            block[index * 4],
            block[index * 4 + 1],
            block[index * 4 + 2],
            block[index * 4 + 3],
        ]);
    }
    for index in 16..64 {
        let s0 =
            w[index - 15].rotate_right(7) ^ w[index - 15].rotate_right(18) ^ (w[index - 15] >> 3);
        let s1 =
            w[index - 2].rotate_right(17) ^ w[index - 2].rotate_right(19) ^ (w[index - 2] >> 10);
        w[index] = w[index - 16]
            .wrapping_add(s0)
            .wrapping_add(w[index - 7])
            .wrapping_add(s1);
    }
    let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) = (
        h[0] as u32,
        h[1] as u32,
        h[2] as u32,
        h[3] as u32,
        h[4] as u32,
        h[5] as u32,
        h[6] as u32,
        h[7] as u32,
    );
    for (index, word) in w.iter().enumerate() {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let temp1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(SHA256_K[index])
            .wrapping_add(*word);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }
    h[0] = h[0].wrapping_add(a as u64);
    h[1] = h[1].wrapping_add(b as u64);
    h[2] = h[2].wrapping_add(c as u64);
    h[3] = h[3].wrapping_add(d as u64);
    h[4] = h[4].wrapping_add(e as u64);
    h[5] = h[5].wrapping_add(f as u64);
    h[6] = h[6].wrapping_add(g as u64);
    h[7] = h[7].wrapping_add(hh as u64);
}

// -------------------------------------------------------------------------
// SHA-512 / SHA-384
// -------------------------------------------------------------------------

const SHA512_K: [u64; 80] = [
    0x428a_2f98_d728_ae22,
    0x7137_4491_23ef_65cd,
    0xb5c0_fbcf_ec4d_3b2f,
    0xe9b5_dba5_8189_dbbc,
    0x3956_c25b_f348_b538,
    0x59f1_11f1_b605_d019,
    0x923f_82a4_af19_4f9b,
    0xab1c_5ed5_da6d_8118,
    0xd807_aa98_a303_0242,
    0x1283_5b01_4570_6fbe,
    0x2431_85be_4ee4_b28c,
    0x550c_7dc3_d5ff_b4e2,
    0x72be_5d74_f27b_896f,
    0x80de_b1fe_3b16_96b1,
    0x9bdc_06a7_25c7_1235,
    0xc19b_f174_cf69_2694,
    0xe49b_69c1_9ef1_4ad2,
    0xefbe_4786_384f_25e3,
    0x0fc1_9dc6_8b8c_d5b5,
    0x240c_a1cc_77ac_9c65,
    0x2de9_2c6f_592b_0275,
    0x4a74_84aa_6ea6_e483,
    0x5cb0_a9dc_bd41_fbd4,
    0x76f9_88da_8311_53b5,
    0x983e_5152_ee66_dfab,
    0xa831_c66d_2db4_3210,
    0xb003_27c8_98fb_213f,
    0xbf59_7fc7_beef_0ee4,
    0xc6e0_0bf3_3da8_8fc2,
    0xd5a7_9147_930a_a725,
    0x06ca_6351_e003_826f,
    0x1429_2967_0a0e_6e70,
    0x27b7_0a85_46d2_2ffc,
    0x2e1b_2138_5c26_c926,
    0x4d2c_6dfc_5ac4_2aed,
    0x5338_0d13_9d95_b3df,
    0x650a_7354_8baf_63de,
    0x766a_0abb_3c77_b2a8,
    0x81c2_c92e_47ed_aee6,
    0x9272_2c85_1482_353b,
    0xa2bf_e8a1_4cf1_0364,
    0xa81a_664b_bc42_3001,
    0xc24b_8b70_d0f8_9791,
    0xc76c_51a3_0654_be30,
    0xd192_e819_d6ef_5218,
    0xd699_0624_5565_a910,
    0xf40e_3585_5771_202a,
    0x106a_a070_32bb_d1b8,
    0x19a4_c116_b8d2_d0c8,
    0x1e37_6c08_5141_ab53,
    0x2748_774c_df8e_eb99,
    0x34b0_bcb5_e19b_48a8,
    0x391c_0cb3_c5c9_5a63,
    0x4ed8_aa4a_e341_8acb,
    0x5b9c_ca4f_7763_e373,
    0x682e_6ff3_d6b2_b8a3,
    0x748f_82ee_5def_b2fc,
    0x78a5_636f_4317_2f60,
    0x84c8_7814_a1f0_ab72,
    0x8cc7_0208_1a64_39ec,
    0x90be_fffa_2363_1e28,
    0xa450_6ceb_de82_bde9,
    0xbef9_a3f7_b2c6_7915,
    0xc671_78f2_e372_532b,
    0xca27_3ece_ea26_619c,
    0xd186_b8c7_21c0_c207,
    0xeada_7dd6_cde0_eb1e,
    0xf57d_4f7f_ee6e_d178,
    0x06f0_67aa_7217_6fba,
    0x0a63_7dc5_a2c8_98a6,
    0x113f_9804_bef9_0dae,
    0x1b71_0b35_131c_471b,
    0x28db_77f5_2304_7d84,
    0x32ca_ab7b_40c7_2493,
    0x3c9e_be0a_15c9_bebc,
    0x431d_67c4_9c10_0d4c,
    0x4cc5_d4be_cb3e_42b6,
    0x597f_299c_fc65_7e2a,
    0x5fcb_6fab_3ad6_faec,
    0x6c44_198c_4a47_5817,
];

fn sha512_compress(h: &mut [u64; 8], block: &[u8]) {
    let mut w = [0u64; 80];
    for (index, word) in w.iter_mut().take(16).enumerate() {
        *word = u64::from_be_bytes([
            block[index * 8],
            block[index * 8 + 1],
            block[index * 8 + 2],
            block[index * 8 + 3],
            block[index * 8 + 4],
            block[index * 8 + 5],
            block[index * 8 + 6],
            block[index * 8 + 7],
        ]);
    }
    for index in 16..80 {
        let s0 =
            w[index - 15].rotate_right(1) ^ w[index - 15].rotate_right(8) ^ (w[index - 15] >> 7);
        let s1 =
            w[index - 2].rotate_right(19) ^ w[index - 2].rotate_right(61) ^ (w[index - 2] >> 6);
        w[index] = w[index - 16]
            .wrapping_add(s0)
            .wrapping_add(w[index - 7])
            .wrapping_add(s1);
    }
    let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
        (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
    for (index, word) in w.iter().enumerate() {
        let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
        let ch = (e & f) ^ (!e & g);
        let temp1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(SHA512_K[index])
            .wrapping_add(*word);
        let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }
    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
    h[5] = h[5].wrapping_add(f);
    h[6] = h[6].wrapping_add(g);
    h[7] = h[7].wrapping_add(hh);
}

// -------------------------------------------------------------------------
// BLAKE2
// -------------------------------------------------------------------------

const BLAKE2_SIGMA: [[usize; 16]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];

#[allow(clippy::too_many_arguments)]
fn blake2b_g(v: &mut [u64; 16], a: usize, b: usize, c: usize, d: usize, x: u64, y: u64) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(32);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(24);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(63);
}

fn blake2b_compress(h: &mut [u64; 8], block: &[u8], t: u128, last: bool, last_node: bool) {
    let mut m = [0u64; 16];
    for (index, word) in m.iter_mut().enumerate() {
        *word = u64_at(block, index * 8);
    }
    let mut v = [0u64; 16];
    v[..8].copy_from_slice(h);
    v[8..].copy_from_slice(&BLAKE2B_IV);
    v[12] ^= t as u64;
    v[13] ^= (t >> 64) as u64;
    if last {
        v[14] = !v[14];
    }
    if last_node {
        v[15] = !v[15];
    }
    for round in 0..12 {
        let s = &BLAKE2_SIGMA[round % 10];
        blake2b_g(&mut v, 0, 4, 8, 12, m[s[0]], m[s[1]]);
        blake2b_g(&mut v, 1, 5, 9, 13, m[s[2]], m[s[3]]);
        blake2b_g(&mut v, 2, 6, 10, 14, m[s[4]], m[s[5]]);
        blake2b_g(&mut v, 3, 7, 11, 15, m[s[6]], m[s[7]]);
        blake2b_g(&mut v, 0, 5, 10, 15, m[s[8]], m[s[9]]);
        blake2b_g(&mut v, 1, 6, 11, 12, m[s[10]], m[s[11]]);
        blake2b_g(&mut v, 2, 7, 8, 13, m[s[12]], m[s[13]]);
        blake2b_g(&mut v, 3, 4, 9, 14, m[s[14]], m[s[15]]);
    }
    for index in 0..8 {
        h[index] ^= v[index] ^ v[index + 8];
    }
}

#[allow(clippy::too_many_arguments)]
fn blake2s_g(v: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, x: u32, y: u32) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(12);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(8);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(7);
}

fn blake2s_compress(h: &mut [u64; 8], block: &[u8], t: u128, last: bool, last_node: bool) {
    let mut m = [0u32; 16];
    for (index, word) in m.iter_mut().enumerate() {
        *word = u32_at(block, index * 4);
    }
    let mut v = [0u32; 16];
    for index in 0..8 {
        v[index] = h[index] as u32;
        v[index + 8] = BLAKE2S_IV[index] as u32;
    }
    v[12] ^= t as u32;
    v[13] ^= (t >> 32) as u32;
    if last {
        v[14] = !v[14];
    }
    if last_node {
        v[15] = !v[15];
    }
    for s in BLAKE2_SIGMA.iter().take(10) {
        blake2s_g(&mut v, 0, 4, 8, 12, m[s[0]], m[s[1]]);
        blake2s_g(&mut v, 1, 5, 9, 13, m[s[2]], m[s[3]]);
        blake2s_g(&mut v, 2, 6, 10, 14, m[s[4]], m[s[5]]);
        blake2s_g(&mut v, 3, 7, 11, 15, m[s[6]], m[s[7]]);
        blake2s_g(&mut v, 0, 5, 10, 15, m[s[8]], m[s[9]]);
        blake2s_g(&mut v, 1, 6, 11, 12, m[s[10]], m[s[11]]);
        blake2s_g(&mut v, 2, 7, 8, 13, m[s[12]], m[s[13]]);
        blake2s_g(&mut v, 3, 4, 9, 14, m[s[14]], m[s[15]]);
    }
    for index in 0..8 {
        h[index] ^= u64::from(v[index] ^ v[index + 8]);
    }
}

// -------------------------------------------------------------------------
// Value helpers and argument binding
// -------------------------------------------------------------------------

fn int_result(number: i64) -> i64 {
    ops::int_result(number)
}

/// Hash constructors and `update` accept any buffer object; strings get the
/// CPython hint instead of the generic buffer error.
fn hash_bytes(arg: i64) -> Result<Vec<u8>, i64> {
    if let Some(bytes) = value::as_bytes(arg) {
        return Ok(bytes.to_vec());
    }
    if let Some(bytes) = value::as_bytearray(arg) {
        return Ok(bytes.clone());
    }
    if let Some(view) = value::as_memoryview(arg) {
        return value::memoryview_bytes(view)
            .map_err(|message| exc::raise_with_message(VALUE_ERROR, message));
    }
    if value::as_str(arg).is_some() {
        return Err(type_error("Strings must be encoded before hashing"));
    }
    Err(type_error("object supporting the buffer API required"))
}

/// Buffer coercion used by `pbkdf2_hmac`, `hmac_*` and `compare_digest`.
fn bytes_like(arg: i64) -> Result<Vec<u8>, i64> {
    if let Some(bytes) = value::as_bytes(arg) {
        return Ok(bytes.to_vec());
    }
    if let Some(bytes) = value::as_bytearray(arg) {
        return Ok(bytes.clone());
    }
    if let Some(view) = value::as_memoryview(arg) {
        return value::memoryview_bytes(view)
            .map_err(|message| exc::raise_with_message(VALUE_ERROR, message));
    }
    Err(type_error(&format!(
        "a bytes-like object is required, not '{}'",
        value::type_name(arg)
    )))
}

fn take_kw(keywords: &mut Vec<(String, i64)>, name: &str) -> Option<i64> {
    let index = keywords.iter().position(|(key, _)| key == name)?;
    Some(keywords.remove(index).1)
}

/// Binds parameter `index` from the positional slots or by name.
fn take(
    args: &[i64],
    keywords: &mut Vec<(String, i64)>,
    index: usize,
    name: &str,
) -> Result<Option<i64>, i64> {
    let positional = args.get(index).copied().filter(|value| *value != NONE);
    let named = take_kw(keywords, name);
    match (positional, named) {
        (Some(_), Some(_)) => Err(type_error(&format!(
            "got multiple values for argument '{name}'"
        ))),
        (Some(value), None) | (None, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

fn take_int(
    args: &[i64],
    keywords: &mut Vec<(String, i64)>,
    index: usize,
    name: &str,
) -> Result<Option<i64>, i64> {
    let Some(value_arg) = take(args, keywords, index, name)? else {
        return Ok(None);
    };
    match value::as_integer(value_arg) {
        Some(number) => Ok(Some(number)),
        None => Err(type_error(&format!(
            "{name} must be an integer, not '{}'",
            value::type_name(value_arg)
        ))),
    }
}

fn take_bytes(
    args: &[i64],
    keywords: &mut Vec<(String, i64)>,
    index: usize,
    name: &str,
) -> Result<Option<Vec<u8>>, i64> {
    let Some(value_arg) = take(args, keywords, index, name)? else {
        return Ok(None);
    };
    hash_bytes(value_arg).map(Some)
}

fn extra_keyword(keywords: &[(String, i64)], function: &str) -> Option<i64> {
    keywords.first().map(|(name, _)| {
        type_error(&format!(
            "{function}() got an unexpected keyword argument '{name}'"
        ))
    })
}

fn truthy(arg: i64) -> bool {
    value::truthy(arg)
}

// -------------------------------------------------------------------------
// Class and instance construction
// -------------------------------------------------------------------------

fn class_with_methods(
    key: &str,
    name: &str,
    specs: &'static [(&'static str, &'static str, usize)],
) -> i64 {
    if let Some(cached) = globals::registry_get(key) {
        return cached;
    }
    let mut methods = Vec::with_capacity(specs.len());
    let mut values = Vec::with_capacity(specs.len());
    for &(method, native, arity) in specs {
        let function = value::native_function_new(MODULE, native, arity);
        values.push(function);
        methods.push((method.to_string(), function, MethodKind::Normal));
    }
    let method_roots = heap::root_values(&values);
    let class = value::class_new_full(name.to_string(), NONE, methods, Vec::new());
    let class_roots = heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(key, class);
    heap::release_roots(class_roots);
    heap::release_roots(method_roots);
    class
}

fn hash_class() -> i64 {
    class_with_methods(HASH_CLASS_KEY, "HASH", &HASH_METHODS)
}

fn hmac_class() -> i64 {
    class_with_methods(HMAC_CLASS_KEY, "HMAC", &HMAC_METHODS)
}

fn store_state(obj: i64, state: &DigestState) {
    let blob = state.serialize();
    let data = value::bytearray_new(blob);
    value::instance_setattr(obj, STATE_ATTR.to_string(), data);
}

fn state_of(obj: i64) -> Option<DigestState> {
    let blob = value::instance_attr(obj, STATE_ATTR)?;
    let bytes = value::as_bytearray(blob)?;
    DigestState::deserialize(bytes)
}

fn store_hmac_blob(obj: i64, attr: &str, state: &DigestState) {
    let data = value::bytearray_new(state.serialize());
    value::instance_setattr(obj, attr.to_string(), data);
}

fn hmac_blob_of(obj: i64, attr: &str) -> Option<DigestState> {
    let blob = value::instance_attr(obj, attr)?;
    let bytes = value::as_bytearray(blob)?;
    DigestState::deserialize(bytes)
}

fn make_hash(state: DigestState) -> i64 {
    let class = hash_class();
    let class_roots = heap::root_values(std::slice::from_ref(&class));
    let obj = value::instance_new(class);
    let roots = heap::root_values(std::slice::from_ref(&obj));
    store_state(obj, &state);
    let name = value::str_alloc(state.algo.name().to_string());
    value::instance_setattr(obj, "name".to_string(), name);
    let digest_size = int_result(state.outlen as i64);
    value::instance_setattr(obj, "digest_size".to_string(), digest_size);
    let block_size = int_result(state.algo.block_size() as i64);
    value::instance_setattr(obj, "block_size".to_string(), block_size);
    heap::release_roots(roots);
    heap::release_roots(class_roots);
    obj
}

fn make_hmac_instance(algo: Algo, inner: DigestState, outer: DigestState, outlen: usize) -> i64 {
    let class = hmac_class();
    let class_roots = heap::root_values(std::slice::from_ref(&class));
    let obj = value::instance_new(class);
    let roots = heap::root_values(std::slice::from_ref(&obj));
    store_hmac_blob(obj, INNER_ATTR, &inner);
    store_hmac_blob(obj, OUTER_ATTR, &outer);
    let name = value::str_alloc(format!("hmac-{}", algo.name()));
    value::instance_setattr(obj, "name".to_string(), name);
    let digest_size = int_result(outlen as i64);
    value::instance_setattr(obj, "digest_size".to_string(), digest_size);
    let block_size = int_result(algo.block_size() as i64);
    value::instance_setattr(obj, "block_size".to_string(), block_size);
    heap::release_roots(roots);
    heap::release_roots(class_roots);
    obj
}

// -------------------------------------------------------------------------
// Hash constructors
// -------------------------------------------------------------------------

fn construct_plain(
    algo: Algo,
    args: &[i64],
    keywords: &mut Vec<(String, i64)>,
    function: &str,
    data_index: usize,
) -> i64 {
    let data = match take(args, keywords, data_index, "data") {
        Ok(data) => data,
        Err(err) => return err,
    };
    let _ = take_kw(keywords, "usedforsecurity");
    if let Some(err) = extra_keyword(keywords, function) {
        return err;
    }
    let data = match data {
        Some(value_arg) => match hash_bytes(value_arg) {
            Ok(data) => data,
            Err(err) => return err,
        },
        None => Vec::new(),
    };
    let mut state = DigestState::new_plain(algo);
    state.update(&data);
    make_hash(state)
}

/// Reads the keyword-only BLAKE2 parameters in CPython's declaration order and
/// validates them before constructing the state.
#[allow(clippy::too_many_arguments)]
fn construct_blake(
    algo: Algo,
    args: &[i64],
    keywords: &mut Vec<(String, i64)>,
    function: &str,
    offset: usize,
) -> i64 {
    let data = match take(args, keywords, offset, "data") {
        Ok(data) => data,
        Err(err) => return err,
    };
    let digest_size = match take_int(args, keywords, offset + 1, "digest_size") {
        Ok(value_arg) => value_arg,
        Err(err) => return err,
    };
    let key = match take_bytes(args, keywords, offset + 2, "key") {
        Ok(value_arg) => value_arg,
        Err(err) => return err,
    };
    let salt = match take_bytes(args, keywords, offset + 3, "salt") {
        Ok(value_arg) => value_arg,
        Err(err) => return err,
    };
    let person = match take_bytes(args, keywords, offset + 4, "person") {
        Ok(value_arg) => value_arg,
        Err(err) => return err,
    };
    let fanout = match take_int(args, keywords, offset + 5, "fanout") {
        Ok(value_arg) => value_arg,
        Err(err) => return err,
    };
    let depth = match take_int(args, keywords, offset + 6, "depth") {
        Ok(value_arg) => value_arg,
        Err(err) => return err,
    };
    let leaf_size = match take_int(args, keywords, offset + 7, "leaf_size") {
        Ok(value_arg) => value_arg,
        Err(err) => return err,
    };
    let node_offset = match take_int(args, keywords, offset + 8, "node_offset") {
        Ok(value_arg) => value_arg,
        Err(err) => return err,
    };
    let node_depth = match take_int(args, keywords, offset + 9, "node_depth") {
        Ok(value_arg) => value_arg,
        Err(err) => return err,
    };
    let inner_size = match take_int(args, keywords, offset + 10, "inner_size") {
        Ok(value_arg) => value_arg,
        Err(err) => return err,
    };
    let last_node = match take(args, keywords, offset + 11, "last_node") {
        Ok(value_arg) => value_arg.map(truthy).unwrap_or(false),
        Err(err) => return err,
    };
    let _ = take_kw(keywords, "usedforsecurity");
    if let Some(err) = extra_keyword(keywords, function) {
        return err;
    }

    let blake_b = algo == Algo::Blake2b;
    let max_digest = algo.digest_size();
    let digest_size = match digest_size {
        Some(size) if size >= 1 && size as usize <= max_digest => size as usize,
        Some(size) => {
            return value_error(&format!(
                "digest_size for {} must be between 1 and {} bytes, here it is {}",
                if blake_b { "Blake2b" } else { "Blake2s" },
                max_digest,
                size
            ))
        }
        None => max_digest,
    };
    let max_salt = if blake_b { 16 } else { 8 };
    let salt = salt.unwrap_or_default();
    if salt.len() > max_salt {
        return value_error(&format!("maximum salt length is {max_salt} bytes"));
    }
    let max_person = if blake_b { 16 } else { 8 };
    let person = person.unwrap_or_default();
    if person.len() > max_person {
        return value_error(&format!("maximum person length is {max_person} bytes"));
    }
    let fanout = match fanout {
        Some(value_arg) if (0..=255).contains(&value_arg) => value_arg as u8,
        Some(_) => return value_error("fanout must be between 0 and 255"),
        None => 1,
    };
    let depth = match depth {
        Some(value_arg) if (1..=255).contains(&value_arg) => value_arg as u8,
        Some(_) => return value_error("depth must be between 1 and 255"),
        None => 1,
    };
    let leaf_size = match leaf_size {
        Some(value_arg) if (0..=u32::MAX as i64).contains(&value_arg) => value_arg as u32,
        Some(_) => return overflow_error("leaf_size is too large"),
        None => 0,
    };
    let node_offset = match node_offset {
        Some(value_arg) if value_arg >= 0 => value_arg as u64,
        Some(_) => return overflow_error("node_offset is too large"),
        None => 0,
    };
    if !blake_b && node_offset > 0xffff_ffff_ffff {
        return overflow_error("node_offset is too large");
    }
    let node_depth = match node_depth {
        Some(value_arg) if (0..=255).contains(&value_arg) => value_arg as u8,
        Some(_) => return value_error("node_depth must be between 0 and 255"),
        None => 0,
    };
    let inner_size = match inner_size {
        Some(value_arg) if (0..=max_digest as i64).contains(&value_arg) => value_arg as u8,
        Some(_) => {
            return value_error(&format!("inner_size must be between 0 and is {max_digest}"))
        }
        None => 0,
    };
    let key = key.unwrap_or_default();
    if key.len() > max_digest {
        return value_error(&format!("maximum key length is {max_digest} bytes"));
    }
    let data = match data {
        Some(value_arg) => match hash_bytes(value_arg) {
            Ok(data) => data,
            Err(err) => return err,
        },
        None => Vec::new(),
    };
    let mut state = DigestState::new_blake2(
        algo,
        digest_size,
        &key,
        &salt,
        &person,
        fanout,
        depth,
        leaf_size,
        node_offset,
        node_depth,
        inner_size,
        last_node,
    );
    state.update(&data);
    make_hash(state)
}

/// Name lookup used by `hashlib.new`/`file_digest`: unlike the constructor
/// functions, the BLAKE2 spellings are lower-case only.
fn new_algorithm(name: &str) -> Option<Algo> {
    let algo = Algo::from_name(name)?;
    if algo.is_blake2() && name != algo.name() {
        return None;
    }
    Some(algo)
}

fn construct(algo: Algo, args: &[i64], keywords: &mut Vec<(String, i64)>) -> i64 {
    if algo.is_blake2() {
        construct_blake(algo, args, keywords, algo.name(), 0)
    } else {
        construct_plain(algo, args, keywords, algo.name(), 0)
    }
}

fn new_call(args: &[i64], keywords: &mut Vec<(String, i64)>) -> i64 {
    let name = match take(args, keywords, 0, "name") {
        Ok(Some(name)) => name,
        Ok(None) => return type_error("new() missing required argument 'name' (pos 1)"),
        Err(err) => return err,
    };
    let Some(text) = value::as_str(name) else {
        return type_error(&format!(
            "new() argument 'name' must be str, not {}",
            value::type_name(name)
        ));
    };
    let Some(algo) = new_algorithm(text) else {
        return value_error(&format!("unsupported hash type {text}"));
    };
    if algo.is_blake2() {
        construct_blake(algo, args, keywords, algo.name(), 1)
    } else {
        construct_plain(algo, args, keywords, "new", 1)
    }
}

// -------------------------------------------------------------------------
// Hash object methods
// -------------------------------------------------------------------------

fn hash_update(args: &[i64]) -> i64 {
    let Some(&obj) = args.first() else {
        return type_error("update() missing required argument 'self'");
    };
    let Some(&data) = args.get(1) else {
        return type_error("update() missing required argument 'data' (pos 1)");
    };
    let Some(mut state) = state_of(obj) else {
        return type_error("update() requires a hashlib hash object");
    };
    let data = match hash_bytes(data) {
        Ok(data) => data,
        Err(err) => return err,
    };
    state.update(&data);
    store_state(obj, &state);
    NONE
}

fn hash_digest(args: &[i64], hex: bool) -> i64 {
    let Some(&obj) = args.first() else {
        return type_error("digest() missing required argument 'self'");
    };
    let Some(state) = state_of(obj) else {
        return type_error("digest() requires a hashlib hash object");
    };
    let digest = state.finalize();
    if hex {
        value::str_alloc(to_hex(&digest))
    } else {
        value::bytes_new(digest)
    }
}

fn hash_copy(args: &[i64]) -> i64 {
    let Some(&obj) = args.first() else {
        return type_error("copy() missing required argument 'self'");
    };
    let Some(state) = state_of(obj) else {
        return type_error("copy() requires a hashlib hash object");
    };
    make_hash(state)
}

fn hmac_update(args: &[i64]) -> i64 {
    let Some(&obj) = args.first() else {
        return type_error("update() missing required argument 'self'");
    };
    let Some(&data) = args.get(1) else {
        return type_error("update() missing required argument 'data' (pos 1)");
    };
    let Some(mut inner) = hmac_blob_of(obj, INNER_ATTR) else {
        return type_error("update() requires a hashlib HMAC object");
    };
    let data = match hash_bytes(data) {
        Ok(data) => data,
        Err(err) => return err,
    };
    inner.update(&data);
    store_hmac_blob(obj, INNER_ATTR, &inner);
    NONE
}

fn hmac_finish(inner: &DigestState, outer: &DigestState) -> Vec<u8> {
    let inner_digest = inner.finalize();
    let mut outer = outer.clone();
    outer.update(&inner_digest);
    outer.finalize()
}

fn hmac_digest(args: &[i64], hex: bool) -> i64 {
    let Some(&obj) = args.first() else {
        return type_error("digest() missing required argument 'self'");
    };
    let (Some(inner), Some(outer)) = (hmac_blob_of(obj, INNER_ATTR), hmac_blob_of(obj, OUTER_ATTR))
    else {
        return type_error("digest() requires a hashlib HMAC object");
    };
    let digest = hmac_finish(&inner, &outer);
    if hex {
        value::str_alloc(to_hex(&digest))
    } else {
        value::bytes_new(digest)
    }
}

fn hmac_copy(args: &[i64]) -> i64 {
    let Some(&obj) = args.first() else {
        return type_error("copy() missing required argument 'self'");
    };
    let (Some(inner), Some(outer)) = (hmac_blob_of(obj, INNER_ATTR), hmac_blob_of(obj, OUTER_ATTR))
    else {
        return type_error("copy() requires a hashlib HMAC object");
    };
    let algo = inner.algo;
    let outlen = inner.outlen;
    make_hmac_instance(algo, inner, outer, outlen)
}

// -------------------------------------------------------------------------
// HMAC and PBKDF2
// -------------------------------------------------------------------------

fn is_callable(arg: i64) -> bool {
    arg != NONE
        && (value::as_function(arg).is_some()
            || value::as_class(arg).is_some()
            || value::as_ext_function(arg).is_some()
            || crate::modules::extension::ext_object_is_callable(arg))
}

fn resolve_digestmod(digestmod: i64) -> Result<Algo, i64> {
    if let Some(name) = value::as_str(digestmod) {
        return Algo::from_name(name)
            .ok_or_else(|| value_error(&format!("unsupported hash type {name}")));
    }
    if is_callable(digestmod) {
        let produced = crate::builtins::call(digestmod, 0, std::ptr::null());
        if exc::pending() {
            return Err(NONE);
        }
        if let Some(state) = state_of(produced) {
            return Ok(state.algo);
        }
        let name = methods::getattr(produced, b"name".as_ptr(), 4);
        if exc::pending() {
            return Err(NONE);
        }
        if let Some(text) = value::as_str(name) {
            if let Some(algo) = Algo::from_name(text) {
                return Ok(algo);
            }
        }
        return Err(type_error(
            "digestmod callable must return a hash object with a known name",
        ));
    }
    Err(type_error("digestmod must be a string or a callable"))
}

/// Derives the two HMAC states (inner and outer pads) for `key`.
fn hmac_states(algo: Algo, key: &[u8]) -> (DigestState, DigestState) {
    let block = algo.block_size();
    let mut key = key.to_vec();
    if key.len() > block {
        let mut state = DigestState::new(algo);
        state.update(&key);
        key = state.finalize();
    }
    key.resize(block, 0);
    let mut inner = DigestState::new(algo);
    let mut outer = DigestState::new(algo);
    let mut pad = [0u8; MAX_BLOCK_BYTES];
    for index in 0..block {
        pad[index] = key[index] ^ 0x36;
    }
    inner.update(&pad[..block]);
    for index in 0..block {
        pad[index] = key[index] ^ 0x5c;
    }
    outer.update(&pad[..block]);
    (inner, outer)
}

fn hmac_new(args: &[i64], keywords: &mut Vec<(String, i64)>) -> i64 {
    let key = match take(args, keywords, 0, "key") {
        Ok(Some(key)) => key,
        Ok(None) => return type_error("hmac_new() missing required argument 'key' (pos 1)"),
        Err(err) => return err,
    };
    let msg = match take(args, keywords, 1, "msg") {
        Ok(msg) => msg.filter(|value_arg| *value_arg != NONE),
        Err(err) => return err,
    };
    let digestmod = match take(args, keywords, 2, "digestmod") {
        Ok(Some(digestmod)) => digestmod,
        Ok(None) => return type_error("Missing required parameter 'digestmod'."),
        Err(err) => return err,
    };
    if let Some(err) = extra_keyword(keywords, "hmac_new") {
        return err;
    }
    let algo = match resolve_digestmod(digestmod) {
        Ok(algo) => algo,
        Err(err) => return err,
    };
    let key = match bytes_like(key) {
        Ok(key) => key,
        Err(err) => return err,
    };
    let msg = match msg {
        Some(msg) => match hash_bytes(msg) {
            Ok(msg) => Some(msg),
            Err(err) => return err,
        },
        None => None,
    };
    let (mut inner, outer) = hmac_states(algo, &key);
    if let Some(msg) = msg {
        inner.update(&msg);
    }
    let outlen = inner.outlen;
    make_hmac_instance(algo, inner, outer, outlen)
}

fn hmac_digest_call(args: &[i64], keywords: &mut Vec<(String, i64)>) -> i64 {
    let key = match take(args, keywords, 0, "key") {
        Ok(Some(key)) => key,
        Ok(None) => return type_error("hmac_digest() missing required argument 'key' (pos 1)"),
        Err(err) => return err,
    };
    let msg = match take(args, keywords, 1, "msg") {
        Ok(Some(msg)) => msg,
        Ok(None) => return type_error("hmac_digest() missing required argument 'msg' (pos 2)"),
        Err(err) => return err,
    };
    let digestmod = match take(args, keywords, 2, "digestmod") {
        Ok(Some(digestmod)) => digestmod,
        Ok(None) => return type_error("Missing required parameter 'digestmod'."),
        Err(err) => return err,
    };
    if let Some(err) = extra_keyword(keywords, "hmac_digest") {
        return err;
    }
    let algo = match resolve_digestmod(digestmod) {
        Ok(algo) => algo,
        Err(err) => return err,
    };
    let key = match bytes_like(key) {
        Ok(key) => key,
        Err(err) => return err,
    };
    let msg = match hash_bytes(msg) {
        Ok(msg) => msg,
        Err(err) => return err,
    };
    let (mut inner, outer) = hmac_states(algo, &key);
    inner.update(&msg);
    value::bytes_new(hmac_finish(&inner, &outer))
}

fn pbkdf2_hmac(algo: Algo, password: &[u8], salt: &[u8], iterations: u32, dklen: usize) -> Vec<u8> {
    let hlen = algo.digest_size();
    let blocks = dklen.div_ceil(hlen);
    let (inner, outer) = hmac_states(algo, password);
    let mut out = Vec::with_capacity(blocks * hlen);
    for block in 1..=blocks {
        let mut salt_block = Vec::with_capacity(salt.len() + 4);
        salt_block.extend_from_slice(salt);
        salt_block.extend_from_slice(&(block as u32).to_be_bytes());
        let mut current = {
            let mut inner = inner.clone();
            inner.update(&salt_block);
            hmac_finish(&inner, &outer)
        };
        let mut accumulated = current.clone();
        for _ in 1..iterations {
            let mut round_inner = inner.clone();
            round_inner.update(&current);
            current = hmac_finish(&round_inner, &outer);
            for (target, source) in accumulated.iter_mut().zip(current.iter()) {
                *target ^= *source;
            }
        }
        out.extend_from_slice(&accumulated);
    }
    out.truncate(dklen);
    out
}

fn pbkdf2_call(args: &[i64], keywords: &mut Vec<(String, i64)>) -> i64 {
    let hash_name = match take(args, keywords, 0, "hash_name") {
        Ok(Some(hash_name)) => hash_name,
        Ok(None) => {
            return type_error("pbkdf2_hmac() missing required argument 'hash_name' (pos 1)")
        }
        Err(err) => return err,
    };
    let password = match take(args, keywords, 1, "password") {
        Ok(Some(password)) => password,
        Ok(None) => {
            return type_error("pbkdf2_hmac() missing required argument 'password' (pos 2)")
        }
        Err(err) => return err,
    };
    let salt = match take(args, keywords, 2, "salt") {
        Ok(Some(salt)) => salt,
        Ok(None) => return type_error("pbkdf2_hmac() missing required argument 'salt' (pos 3)"),
        Err(err) => return err,
    };
    let iterations = match take_int(args, keywords, 3, "iterations") {
        Ok(Some(iterations)) => iterations,
        Ok(None) => {
            return type_error("pbkdf2_hmac() missing required argument 'iterations' (pos 4)")
        }
        Err(err) => return err,
    };
    let dklen = match take_int(args, keywords, 4, "dklen") {
        Ok(value_arg) => value_arg,
        Err(err) => return err,
    };
    if let Some(err) = extra_keyword(keywords, "pbkdf2_hmac") {
        return err;
    }
    let Some(name) = value::as_str(hash_name) else {
        return type_error(&format!(
            "hash_name must be str, not {}",
            value::type_name(hash_name)
        ));
    };
    let Some(algo) = Algo::from_name(name) else {
        return value_error(&format!("unsupported hash type {name}"));
    };
    if iterations < 1 {
        return value_error("iteration value must be greater than 0.");
    }
    if iterations > i32::MAX as i64 {
        return overflow_error("iteration value is too great.");
    }
    let dklen = match dklen {
        Some(dklen) if dklen >= 1 => dklen as usize,
        Some(_) => return value_error("key length must be greater than 0."),
        None => algo.digest_size(),
    };
    if dklen > i32::MAX as usize {
        return overflow_error("key length is too great.");
    }
    let password = match bytes_like(password) {
        Ok(password) => password,
        Err(err) => return err,
    };
    let salt = match bytes_like(salt) {
        Ok(salt) => salt,
        Err(err) => return err,
    };
    value::bytes_new(pbkdf2_hmac(
        algo,
        &password,
        &salt,
        iterations as u32,
        dklen,
    ))
}

// -------------------------------------------------------------------------
// compare_digest and file_digest
// -------------------------------------------------------------------------

fn compare_digest(args: &[i64], keywords: &mut Vec<(String, i64)>) -> i64 {
    let a = match take(args, keywords, 0, "a") {
        Ok(Some(a)) => a,
        Ok(None) => return type_error("compare_digest() missing required argument 'a' (pos 1)"),
        Err(err) => return err,
    };
    let b = match take(args, keywords, 1, "b") {
        Ok(Some(b)) => b,
        Ok(None) => return type_error("compare_digest() missing required argument 'b' (pos 2)"),
        Err(err) => return err,
    };
    if let Some(err) = extra_keyword(keywords, "compare_digest") {
        return err;
    }
    let left_text = value::as_str(a);
    let right_text = value::as_str(b);
    let (left, right) = if let (Some(left), Some(right)) = (left_text, right_text) {
        if !left.is_ascii() || !right.is_ascii() {
            return type_error("comparing strings with non-ASCII characters is not supported");
        }
        (left.as_bytes().to_vec(), right.as_bytes().to_vec())
    } else if left_text.is_some() || right_text.is_some() {
        return type_error("a bytes-like object is required, not 'str'");
    } else {
        let left = match bytes_like(a) {
            Ok(bytes) => bytes,
            Err(err) => return err,
        };
        let right = match bytes_like(b) {
            Ok(bytes) => bytes,
            Err(err) => return err,
        };
        (left, right)
    };
    let mut difference = usize::from(left.len() != right.len());
    let length = left.len().max(right.len());
    for index in 0..length {
        let l = left.get(index).copied().unwrap_or(0);
        let r = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(l ^ r);
    }
    if difference == 0 {
        value::TRUE
    } else {
        value::FALSE
    }
}

fn try_callable_attr(obj: i64, name: &str) -> Option<i64> {
    let attr = methods::getattr(obj, name.as_ptr(), name.len());
    if exc::pending() {
        let _ = exc::clear();
        return None;
    }
    is_callable(attr).then_some(attr)
}

fn call_no_args(function: i64) -> i64 {
    crate::builtins::call(function, 0, std::ptr::null())
}

fn update_digest_object(obj: i64, data: &[u8]) -> i64 {
    if let Some(mut state) = state_of(obj) {
        state.update(data);
        store_state(obj, &state);
        return NONE;
    }
    let Some(update) = try_callable_attr(obj, "update") else {
        return exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!(
                "'{}' object has no attribute 'update'",
                value::type_name(obj)
            ),
        );
    };
    let data = value::bytes_new(data.to_vec());
    let arguments = [data];
    let _ = crate::builtins::call(update, 1, arguments.as_ptr());
    NONE
}

fn not_binary_file(fileobj: i64) -> i64 {
    value_error(&format!(
        "'{0}' is not a file-like object in binary reading mode.",
        value::repr(fileobj).trim_matches('\'')
    ))
}

fn file_digest(args: &[i64], keywords: &mut Vec<(String, i64)>) -> i64 {
    let fileobj = match take(args, keywords, 0, "fileobj") {
        Ok(Some(fileobj)) => fileobj,
        Ok(None) => return type_error("file_digest() missing required argument 'fileobj' (pos 1)"),
        Err(err) => return err,
    };
    let digest = match take(args, keywords, 1, "digest") {
        Ok(Some(digest)) => digest,
        Ok(None) => return type_error("file_digest() missing required argument 'digest' (pos 2)"),
        Err(err) => return err,
    };
    let buf_size = match take_int(args, keywords, 2, "_bufsize") {
        Ok(Some(size)) if size > 0 => size as usize,
        Ok(Some(_)) => return value_error("_bufsize must be positive"),
        Ok(None) => 2usize.pow(18),
        Err(err) => return err,
    };
    if let Some(err) = extra_keyword(keywords, "file_digest") {
        return err;
    }
    let digest_obj = if let Some(name) = value::as_str(digest) {
        let Some(algo) = new_algorithm(name) else {
            return value_error(&format!("unsupported hash type {name}"));
        };
        let state = DigestState::new(algo);
        make_hash(state)
    } else if is_callable(digest) {
        let produced = call_no_args(digest);
        if exc::pending() {
            return NONE;
        }
        produced
    } else {
        return type_error("digest must be a string or a callable returning a hash object");
    };
    let digest_roots = heap::root_values(std::slice::from_ref(&digest_obj));

    if let Some(getbuffer) = try_callable_attr(fileobj, "getbuffer") {
        let buffer = call_no_args(getbuffer);
        if exc::pending() {
            heap::release_roots(digest_roots);
            return NONE;
        }
        let data = match hash_bytes(buffer) {
            Ok(data) => data,
            Err(err) => {
                heap::release_roots(digest_roots);
                return err;
            }
        };
        let result = update_digest_object(digest_obj, &data);
        heap::release_roots(digest_roots);
        return if exc::pending() { NONE } else { result };
    }

    if let Some(readable) = try_callable_attr(fileobj, "readable") {
        let flag = call_no_args(readable);
        if exc::pending() {
            heap::release_roots(digest_roots);
            return NONE;
        }
        if !truthy(flag) {
            let err = not_binary_file(fileobj);
            heap::release_roots(digest_roots);
            return err;
        }
    }

    if let Some(readinto) = try_callable_attr(fileobj, "readinto") {
        let buffer = value::bytearray_new(vec![0u8; buf_size]);
        let buffer_roots = heap::root_values(std::slice::from_ref(&buffer));
        let view = value::memoryview_abi(buffer);
        let view_roots = heap::root_values(std::slice::from_ref(&view));
        loop {
            let arguments = [view];
            let read = crate::builtins::call(readinto, 1, arguments.as_ptr());
            if exc::pending() {
                heap::release_roots(view_roots);
                heap::release_roots(buffer_roots);
                heap::release_roots(digest_roots);
                return NONE;
            }
            if read == NONE {
                let err = exc::raise_with_message(BLOCKING_IO_ERROR, "I/O operation would block.");
                heap::release_roots(view_roots);
                heap::release_roots(buffer_roots);
                heap::release_roots(digest_roots);
                return err;
            }
            let Some(size) = value::as_integer(read) else {
                let err = type_error("readinto() returned non-integer");
                heap::release_roots(view_roots);
                heap::release_roots(buffer_roots);
                heap::release_roots(digest_roots);
                return err;
            };
            if size == 0 {
                break;
            }
            let Some(bytes) = value::as_bytearray(buffer) else {
                let err = type_error("file buffer is unavailable");
                heap::release_roots(view_roots);
                heap::release_roots(buffer_roots);
                heap::release_roots(digest_roots);
                return err;
            };
            let end = (size as usize).min(bytes.len());
            update_digest_object(digest_obj, &bytes[..end]);
            if exc::pending() {
                heap::release_roots(view_roots);
                heap::release_roots(buffer_roots);
                heap::release_roots(digest_roots);
                return NONE;
            }
        }
        heap::release_roots(view_roots);
        heap::release_roots(buffer_roots);
        heap::release_roots(digest_roots);
        return digest_obj;
    }

    let Some(read) = try_callable_attr(fileobj, "read") else {
        let err = not_binary_file(fileobj);
        heap::release_roots(digest_roots);
        return err;
    };
    loop {
        let size = int_result(buf_size as i64);
        let arguments = [size];
        let chunk = crate::builtins::call(read, 1, arguments.as_ptr());
        if exc::pending() {
            heap::release_roots(digest_roots);
            return NONE;
        }
        if chunk == NONE {
            let err = exc::raise_with_message(BLOCKING_IO_ERROR, "I/O operation would block.");
            heap::release_roots(digest_roots);
            return err;
        }
        if value::as_str(chunk).is_some() {
            let err = not_binary_file(fileobj);
            heap::release_roots(digest_roots);
            return err;
        }
        let data = match hash_bytes(chunk) {
            Ok(data) => data,
            Err(err) => {
                heap::release_roots(digest_roots);
                return err;
            }
        };
        if data.is_empty() {
            break;
        }
        update_digest_object(digest_obj, &data);
        if exc::pending() {
            heap::release_roots(digest_roots);
            return NONE;
        }
    }
    heap::release_roots(digest_roots);
    digest_obj
}

// -------------------------------------------------------------------------
// UUID helpers
// -------------------------------------------------------------------------

const NAMESPACE_DNS: [u8; 16] = [
    0x6b, 0xa7, 0xb8, 0x10, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
];
const NAMESPACE_URL: [u8; 16] = [
    0x6b, 0xa7, 0xb8, 0x11, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
];
const NAMESPACE_OID: [u8; 16] = [
    0x6b, 0xa7, 0xb8, 0x12, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
];
const NAMESPACE_X500: [u8; 16] = [
    0x6b, 0xa7, 0xb8, 0x14, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
];

static LAST_UUID_TIMESTAMP: Mutex<u64> = Mutex::new(0);

/// Canonical RFC 9562 text form (`8-4-4-4-12` lower-case hex).
fn uuid_display(bytes: &[u8; 16]) -> String {
    let hex = to_hex(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Accepts the canonical form, a bare 32-digit hex string, and the optional
/// braces used by `uuid.UUID`.
fn uuid_parse(text: &str) -> Option<[u8; 16]> {
    let mut digits = [0u8; 32];
    let mut len = 0usize;
    for byte in text.bytes() {
        let digit = match byte {
            b'-' | b'{' | b'}' => continue,
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return None,
        };
        if len == digits.len() {
            return None;
        }
        digits[len] = digit;
        len += 1;
    }
    if len != digits.len() {
        return None;
    }
    let mut out = [0u8; 16];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = (digits[index * 2] << 4) | digits[index * 2 + 1];
    }
    Some(out)
}

fn entropy() -> Result<[u8; 16], i64> {
    let mut bytes = [0u8; 16];
    value::fill_entropy(&mut bytes).map_err(|error| {
        exc::raise_with_message(OS_ERROR, &format!("entropy source failed: {error}"))
    })?;
    Ok(bytes)
}

fn uuid1_bytes(node: Option<u64>, clock_seq: Option<u16>) -> Result<[u8; 16], i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| exc::raise_with_message(OS_ERROR, &format!("clock failed: {error}")))?;
    // 100 ns intervals between 1582-10-15 and 1970-01-01.
    let timestamp = duration.as_nanos() / 100 + 0x01b2_1dd2_1381_4000;
    let mut timestamp = timestamp.min(u64::MAX as u128) as u64;
    {
        let mut last = LAST_UUID_TIMESTAMP
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if timestamp <= *last {
            timestamp = last.saturating_add(1);
        }
        *last = timestamp;
    }
    let random = entropy()?;
    let node = node.unwrap_or_else(|| {
        let mut value = u64::from_be_bytes([
            0, 0, random[0], random[1], random[2], random[3], random[4], random[5],
        ]);
        value |= 1 << 40;
        value
    });
    let clock_seq =
        clock_seq.unwrap_or_else(|| u16::from_be_bytes([random[6], random[7]]) & 0x3fff);
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&((timestamp & 0xffff_ffff) as u32).to_be_bytes());
    out[4..6].copy_from_slice(&(((timestamp >> 32) & 0xffff) as u16).to_be_bytes());
    let high = ((((timestamp >> 48) & 0x0fff) as u16) | 0x1000).to_be_bytes();
    out[6..8].copy_from_slice(&high);
    out[8] = 0x80 | ((clock_seq >> 8) as u8 & 0x3f);
    out[9] = (clock_seq & 0xff) as u8;
    out[10..16].copy_from_slice(&node.to_be_bytes()[2..8]);
    Ok(out)
}

fn uuid4_bytes() -> Result<[u8; 16], i64> {
    let mut out = entropy()?;
    out[6] = (out[6] & 0x0f) | 0x40;
    out[8] = (out[8] & 0x3f) | 0x80;
    Ok(out)
}

fn uuid5_bytes(namespace: &[u8], name: &[u8]) -> [u8; 16] {
    let mut state = DigestState::new(Algo::Sha1);
    state.update(namespace);
    state.update(name);
    let digest = state.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out[6] = (out[6] & 0x0f) | 0x50;
    out[8] = (out[8] & 0x3f) | 0x80;
    out
}

fn uuid1_call(args: &[i64], keywords: &mut Vec<(String, i64)>) -> i64 {
    let node = match take_int(args, keywords, 0, "node") {
        Ok(value_arg) => value_arg,
        Err(err) => return err,
    };
    let clock_seq = match take_int(args, keywords, 1, "clock_seq") {
        Ok(value_arg) => value_arg,
        Err(err) => return err,
    };
    if let Some(err) = extra_keyword(keywords, "uuid1") {
        return err;
    }
    let node = match node {
        Some(node) if (0..(1i64 << 48)).contains(&node) => Some(node as u64),
        Some(_) => return value_error("node must be a 48-bit integer"),
        None => None,
    };
    let clock_seq = match clock_seq {
        Some(value_arg) if (0..0x4000).contains(&value_arg) => Some(value_arg as u16),
        Some(_) => return value_error("clock_seq must be a 14-bit integer"),
        None => None,
    };
    match uuid1_bytes(node, clock_seq) {
        Ok(bytes) => value::str_alloc(uuid_display(&bytes)),
        Err(err) => err,
    }
}

fn uuid4_call(args: &[i64], keywords: &mut [(String, i64)]) -> i64 {
    if let Some(err) = extra_keyword(keywords, "uuid4") {
        return err;
    }
    if !args.is_empty() {
        return type_error(&format!(
            "uuid4() takes no arguments ({} given)",
            args.len()
        ));
    }
    match uuid4_bytes() {
        Ok(bytes) => value::str_alloc(uuid_display(&bytes)),
        Err(err) => err,
    }
}

fn uuid5_call(args: &[i64], keywords: &mut Vec<(String, i64)>) -> i64 {
    let namespace = match take(args, keywords, 0, "namespace") {
        Ok(Some(namespace)) => namespace,
        Ok(None) => return type_error("uuid5() missing required argument 'namespace' (pos 1)"),
        Err(err) => return err,
    };
    let name = match take(args, keywords, 1, "name") {
        Ok(Some(name)) => name,
        Ok(None) => return type_error("uuid5() missing required argument 'name' (pos 2)"),
        Err(err) => return err,
    };
    if let Some(err) = extra_keyword(keywords, "uuid5") {
        return err;
    }
    let namespace = if let Some(text) = value::as_str(namespace) {
        match uuid_parse(text) {
            Some(namespace) => namespace,
            None => return value_error("namespace must be a 16-byte UUID or UUID string"),
        }
    } else {
        match bytes_like(namespace) {
            Ok(bytes) if bytes.len() == 16 => {
                let mut namespace = [0u8; 16];
                namespace.copy_from_slice(&bytes);
                namespace
            }
            Ok(_) => return value_error("namespace must be a 16-byte UUID"),
            Err(err) => return err,
        }
    };
    let name = if let Some(text) = value::as_str(name) {
        text.as_bytes().to_vec()
    } else {
        match bytes_like(name) {
            Ok(bytes) => bytes,
            Err(err) => return err,
        }
    };
    value::str_alloc(uuid_display(&uuid5_bytes(&namespace, &name)))
}

fn uuid_generate_time_safe(args: &[i64], keywords: &mut [(String, i64)]) -> i64 {
    if let Some(err) = extra_keyword(keywords, "generate_time_safe") {
        return err;
    }
    if !args.is_empty() {
        return type_error(&format!(
            "generate_time_safe() takes no arguments ({} given)",
            args.len()
        ));
    }
    match uuid1_bytes(None, None) {
        Ok(bytes) => {
            let bytes = value::bytes_new(bytes.to_vec());
            let safe = int_result(1);
            value::tuple_new(vec![bytes, safe])
        }
        Err(err) => err,
    }
}

// -------------------------------------------------------------------------
// Module surface
// -------------------------------------------------------------------------

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn algorithm_set() -> i64 {
    let names = [
        "md5", "sha1", "sha224", "sha256", "sha384", "sha512", "blake2b", "blake2s",
    ];
    let items = names
        .iter()
        .map(|name| value::str_alloc((*name).to_string()))
        .collect();
    value::set_new(items)
}

/// Alias-aware constant lookup. `module` distinguishes `_hmac`'s
/// `UnknownHashError` and the `_uuid` capability flags.
pub(crate) fn constant_named(module: &str, name: &str) -> Option<i64> {
    Some(match name {
        "algorithms_guaranteed" => algorithm_set(),
        "algorithms_available" => algorithm_set(),
        "openssl_md_meth_names" => algorithm_set(),
        "HASH" if matches!(module, "hashlib" | "_hashlib") => hash_class(),
        "HMAC" if matches!(module, "hashlib" | "_hashlib" | "_hmac") => hmac_class(),
        "UnsupportedDigestmodError" if matches!(module, "hashlib" | "_hashlib") => {
            exc::exception_class(VALUE_ERROR)
        }
        "UnknownHashError" if module == "_hmac" => exc::exception_class(VALUE_ERROR),
        "NAMESPACE_DNS" => value::str_alloc(uuid_display(&NAMESPACE_DNS)),
        "NAMESPACE_URL" => value::str_alloc(uuid_display(&NAMESPACE_URL)),
        "NAMESPACE_OID" => value::str_alloc(uuid_display(&NAMESPACE_OID)),
        "NAMESPACE_X500" => value::str_alloc(uuid_display(&NAMESPACE_X500)),
        "has_uuid_generate_time_safe" if module == "_uuid" => value::TRUE,
        "has_stable_extractable_node" if module == "_uuid" => value::FALSE,
        _ => return None,
    })
}

fn openssl_constructor(bare: &str) -> Option<(Algo, Option<usize>)> {
    Some(match bare {
        "openssl_md5" => (Algo::Md5, None),
        "openssl_sha1" => (Algo::Sha1, None),
        "openssl_sha224" => (Algo::Sha224, None),
        "openssl_sha256" => (Algo::Sha256, None),
        "openssl_sha384" => (Algo::Sha384, None),
        "openssl_sha512" => (Algo::Sha512, None),
        "openssl_blake2b512" => (Algo::Blake2b, Some(64)),
        "openssl_blake2s256" => (Algo::Blake2s, Some(32)),
        _ => return None,
    })
}

fn method_dispatch(function: &str, args: &[i64], keywords: &[(String, i64)]) -> Option<i64> {
    let is_method = matches!(
        function,
        "hashlib.update"
            | "hashlib.digest"
            | "hashlib.hexdigest"
            | "hashlib.copy"
            | "hashlib.HMAC.update"
            | "hashlib.HMAC.digest"
            | "hashlib.HMAC.hexdigest"
            | "hashlib.HMAC.copy"
    );
    if !is_method {
        return None;
    }
    if !keywords.is_empty() {
        let receiver = if function.starts_with("hashlib.HMAC.") {
            "HMAC"
        } else {
            "HASH"
        };
        return Some(type_error(&format!(
            "{receiver}.{}() takes no keyword arguments",
            function.rsplit('.').next().unwrap_or(function)
        )));
    }
    Some(match function {
        "hashlib.update" => hash_update(args),
        "hashlib.digest" => hash_digest(args, false),
        "hashlib.hexdigest" => hash_digest(args, true),
        "hashlib.copy" => hash_copy(args),
        "hashlib.HMAC.update" => hmac_update(args),
        "hashlib.HMAC.digest" => hmac_digest(args, false),
        "hashlib.HMAC.hexdigest" => hmac_digest(args, true),
        "hashlib.HMAC.copy" => hmac_copy(args),
        _ => unreachable!(),
    })
}

/// Bare method names arrive when the class table stores the short native name
/// (`update`, ...). The receiver's hidden state selects HASH vs HMAC.
fn bare_method_dispatch(function: &str, args: &[i64], keywords: &[(String, i64)]) -> Option<i64> {
    if !matches!(function, "update" | "digest" | "hexdigest" | "copy") {
        return None;
    }
    if !keywords.is_empty() {
        return Some(type_error(&format!(
            "{function}() takes no keyword arguments"
        )));
    }
    let Some(&obj) = args.first() else {
        return Some(type_error(&format!(
            "{function}() missing required argument 'self'"
        )));
    };
    let hashed = value::instance_attr(obj, STATE_ATTR).is_some();
    let mac = value::instance_attr(obj, INNER_ATTR).is_some();
    if hashed {
        Some(match function {
            "update" => hash_update(args),
            "digest" => hash_digest(args, false),
            "hexdigest" => hash_digest(args, true),
            "copy" => hash_copy(args),
            _ => unreachable!(),
        })
    } else if mac {
        Some(match function {
            "update" => hmac_update(args),
            "digest" => hmac_digest(args, false),
            "hexdigest" => hmac_digest(args, true),
            "copy" => hmac_copy(args),
            _ => unreachable!(),
        })
    } else {
        Some(type_error("method requires a hashlib hash or HMAC object"))
    }
}

/// Calls one `hashlib` module function with positional arguments only.
#[cfg(test)]
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    call_named(MODULE, function, args)
}

/// Keyword-aware `hashlib` call entry point.
#[cfg(test)]
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    call_named_kw(MODULE, function, args, keywords)
}

pub(crate) fn call_named(module: &str, function: &str, args: &[i64]) -> i64 {
    call_named_kw(module, function, args, &[])
}

/// Keyword-aware dispatch for `hashlib.new`, the constructors, `pbkdf2_hmac`,
/// `file_digest`, `hmac_new` and the UUID helpers.
pub(crate) fn call_named_kw(
    module: &str,
    function: &str,
    args: &[i64],
    keywords: &[(String, i64)],
) -> i64 {
    if let Some(result) = method_dispatch(function, args, keywords) {
        return result;
    }
    if let Some(result) = bare_method_dispatch(function, args, keywords) {
        return result;
    }
    let mut keywords = keywords.to_vec();
    let bare = function.rsplit('.').next().unwrap_or(function);
    if module == "_hmac" && (bare == "new" || bare == "compute_digest") {
        return if bare == "new" {
            hmac_new(args, &mut keywords)
        } else {
            hmac_digest_call(args, &mut keywords)
        };
    }
    if module == "_uuid" && bare == "generate_time_safe" {
        return uuid_generate_time_safe(args, &mut keywords);
    }
    match bare {
        "new" => new_call(args, &mut keywords),
        "md5" => construct(Algo::Md5, args, &mut keywords),
        "sha1" => construct(Algo::Sha1, args, &mut keywords),
        "sha224" => construct(Algo::Sha224, args, &mut keywords),
        "sha256" => construct(Algo::Sha256, args, &mut keywords),
        "sha384" => construct(Algo::Sha384, args, &mut keywords),
        "sha512" => construct(Algo::Sha512, args, &mut keywords),
        "blake2b" => construct_blake(Algo::Blake2b, args, &mut keywords, "blake2b", 0),
        "blake2s" => construct_blake(Algo::Blake2s, args, &mut keywords, "blake2s", 0),
        "pbkdf2_hmac" => pbkdf2_call(args, &mut keywords),
        "compare_digest" => compare_digest(args, &mut keywords),
        "hmac_new" => hmac_new(args, &mut keywords),
        "hmac_digest" | "compute_digest" => hmac_digest_call(args, &mut keywords),
        "file_digest" => file_digest(args, &mut keywords),
        "get_fips_mode" => {
            if let Some(err) = extra_keyword(&keywords, "get_fips_mode") {
                err
            } else {
                int_result(0)
            }
        }
        "_is_fips_mode" => {
            if let Some(err) = extra_keyword(&keywords, "_is_fips_mode") {
                err
            } else {
                value::FALSE
            }
        }
        "uuid1" => uuid1_call(args, &mut keywords),
        "uuid4" => uuid4_call(args, &mut keywords),
        "uuid5" => uuid5_call(args, &mut keywords),
        "generate_time_safe" => uuid_generate_time_safe(args, &mut keywords),
        _ => match openssl_constructor(bare) {
            Some((algo, _forced_size)) => {
                if algo.is_blake2() {
                    construct_blake(algo, args, &mut keywords, bare, 0)
                } else {
                    construct_plain(algo, args, &mut keywords, bare, 0)
                }
            }
            None => attribute_error(module, function),
        },
    }
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value_arg: &str) -> i64 {
        value::str_alloc(value_arg.to_string())
    }

    fn bytes(value_arg: &[u8]) -> i64 {
        value::bytes_new(value_arg.to_vec())
    }

    fn as_int(value_arg: i64) -> i64 {
        value::as_integer(value_arg).expect("int result")
    }

    fn digest_hex(algo: Algo, data: &[u8]) -> String {
        let mut state = DigestState::new(algo);
        state.update(data);
        to_hex(&state.finalize())
    }

    fn hmac_hex(algo: Algo, key: &[u8], msg: &[u8]) -> String {
        let (mut inner, outer) = hmac_states(algo, key);
        inner.update(msg);
        to_hex(&hmac_finish(&inner, &outer))
    }

    fn clear_error() -> u32 {
        let tag = exc::pending_tag() as u32;
        exc::clear();
        tag
    }

    #[test]
    fn known_digest_vectors() {
        assert_eq!(
            digest_hex(Algo::Md5, b""),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
        assert_eq!(
            digest_hex(Algo::Md5, b"abc"),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        assert_eq!(
            digest_hex(Algo::Sha1, b"abc"),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            digest_hex(Algo::Sha1, b""),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            digest_hex(Algo::Sha224, b"abc"),
            "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7"
        );
        assert_eq!(
            digest_hex(Algo::Sha224, b""),
            "d14a028c2a3a2bc9476102bb288234c415a2b01f828ea62ac5b3e42f"
        );
        assert_eq!(
            digest_hex(Algo::Sha256, b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            digest_hex(Algo::Sha256, b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest_hex(Algo::Sha384, b"abc"),
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed\
8086072ba1e7cc2358baeca134c825a7"
        );
        assert_eq!(
            digest_hex(Algo::Sha384, b""),
            "38b060a751ac96384cd9327eb1b1e36a21fdb71114be07434c0cc7bf63f6e1da\
274edebfe76f65fbd51ad2f14898b95b"
        );
        assert_eq!(
            digest_hex(Algo::Sha512, b"abc"),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
        assert_eq!(
            digest_hex(Algo::Sha512, b""),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce\
47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
        assert_eq!(
            digest_hex(Algo::Blake2b, b""),
            "786a02f742015903c6c6fd852552d272912f4740e15847618a86e217f71f5419\
d25e1031afee585313896444934eb04b903a685b1448b755d56f701afe9be2ce"
        );
        assert_eq!(
            digest_hex(Algo::Blake2b, b"abc"),
            "ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d1\
7d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd4009923"
        );
        assert_eq!(
            digest_hex(Algo::Blake2s, b""),
            "69217a3079908094e11121d042354a7c1f55b6482ca1a51e1b250dfd1ed0eef9"
        );
        assert_eq!(
            digest_hex(Algo::Blake2s, b"abc"),
            "508c5e8c327c14e2e1a72ba34eeb452f37458b209ed63a294d999b4c86675982"
        );
    }

    #[test]
    fn multi_block_vectors() {
        let long = vec![b'a'; 200];
        assert_eq!(
            digest_hex(Algo::Md5, &long),
            "887f30b43b2867f4a9accceee7d16e6c"
        );
        assert_eq!(
            digest_hex(Algo::Sha1, &long),
            "e61cfffe0d9195a525fc6cf06ca2d77119c24a40"
        );
        assert_eq!(
            digest_hex(Algo::Sha256, &long),
            "c2a908d98f5df987ade41b5fce213067efbcc21ef2240212a41e54b5e7c28ae5"
        );
        assert_eq!(
            digest_hex(Algo::Sha384, &long),
            "0691b6e978614b67d60557b2a2cddd53406508522efa21c624dbbfa8ab6e726d\
5c586b489c7c09f24109a64c10211d48"
        );
        assert_eq!(
            digest_hex(Algo::Sha512, &long),
            "4b11459c33f52a22ee8236782714c150a3b2c60994e9acee17fe68947a3e6789\
f31e7668394592da7bef827cddca88c4e6f86e4df7ed1ae6cba71f3e98faee9f"
        );
        assert_eq!(
            digest_hex(Algo::Blake2b, &vec![b'a'; 300]),
            "a2ff3040eda405b929c2fc2fd93e8add6ac3bb5369b679bae170ac6956863ca0\
06285f132a868000fc3fae5bc696e5d17fe3fddfb4a342876c40451184742986"
        );
        assert_eq!(
            digest_hex(Algo::Blake2s, &[b'a'; 100]),
            "214f24fe1118eb854450238e11bebe22d2e3937ed85c7c96c6c010106b752ad3"
        );
    }

    #[test]
    fn streaming_and_copy_round_trip() {
        for algo in [
            Algo::Md5,
            Algo::Sha1,
            Algo::Sha224,
            Algo::Sha256,
            Algo::Sha384,
            Algo::Sha512,
            Algo::Blake2b,
            Algo::Blake2s,
        ] {
            let whole = digest_hex(algo, b"abcdefghijklmnopqrstuvwxyz0123456789");
            let mut state = DigestState::new(algo);
            for chunk in [
                &b"abcdefghijklm"[..],
                &b"nopqrstuvwxyz"[..],
                &b"0123456789"[..],
            ] {
                state.update(chunk);
            }
            assert_eq!(to_hex(&state.finalize()), whole, "{algo:?} streaming");

            // The serialized form resumes to the same digest.
            let mut resumed = DigestState::deserialize(&state.serialize()).expect("state");
            resumed.update(b"!");
            let mut expected = state.clone();
            expected.update(b"!");
            assert_eq!(resumed.finalize(), expected.finalize());

            // Finalizing does not mutate the state; a subsequent update still
            // extends the original stream.
            let first = state.finalize();
            state.update(b"!");
            assert_ne!(first, state.finalize());
        }
    }

    #[test]
    fn blake2_key_and_tree_parameters() {
        let mut state =
            DigestState::new_blake2(Algo::Blake2b, 64, b"key", b"", b"", 1, 1, 0, 0, 0, 0, false);
        state.update(b"abc");
        assert_eq!(
            to_hex(&state.finalize()),
            "5c6a9a4ae911c02fb7e71a991eb9aea371ae993d4842d206e6020d46f5e41358\
c6d5c277c110ef86c959ed63e6ecaaaceaaff38019a43264ae06acf73b9550b1"
        );

        let mut state = DigestState::new_blake2(
            Algo::Blake2s,
            32,
            b"",
            b"saltsalt",
            b"personpe",
            1,
            1,
            0,
            0,
            0,
            0,
            false,
        );
        state.update(b"abc");
        assert_eq!(
            to_hex(&state.finalize()),
            "a39282cabcad2b066089589094f538465370e8a0750f8f9955b286c0545e5804"
        );

        let mut state =
            DigestState::new_blake2(Algo::Blake2b, 64, b"", b"", b"", 2, 2, 4096, 1, 1, 32, true);
        state.update(b"abc");
        assert_eq!(
            to_hex(&state.finalize()),
            "82d2c82dd177bd3b777908e4d48615f219de2509927cf931af6981416006fe78\
3dde461232cb06ec8570fb7dd6d5674fc3f653ac3233861dc8133ae3771f5d29"
        );
    }

    #[test]
    fn hmac_vectors() {
        assert_eq!(
            hmac_hex(Algo::Sha256, &[0x0b; 20], b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hmac_hex(Algo::Md5, &[0x0b; 16], b"Hi There"),
            "9294727a3638bb1c13f48ef8158bfc9d"
        );
        assert_eq!(
            hmac_hex(Algo::Sha1, &[0x0b; 20], b"Hi There"),
            "b617318655057264e28bc0b6fb378c8ef146be00"
        );
        assert_eq!(
            hmac_hex(Algo::Blake2b, b"key", b"msg"),
            "87e0b1bf02148b65ea0184887d84f6a601a07c1deef251ab2593ea43a93a4478\
3aa640a1faeb6897811544b07ee2af0efa705d21ec27b3b33a1aac53e0d1f49f"
        );
    }

    #[test]
    fn pbkdf2_vectors() {
        assert_eq!(
            to_hex(&pbkdf2_hmac(Algo::Sha256, b"password", b"salt", 1, 32)),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        assert_eq!(
            to_hex(&pbkdf2_hmac(Algo::Sha256, b"password", b"salt", 2, 32)),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
        assert_eq!(
            to_hex(&pbkdf2_hmac(Algo::Sha1, b"password", b"salt", 1, 20)),
            "0c60c80f961f0e71f3a9b524af6012062fe037a6"
        );
        assert_eq!(
            to_hex(&pbkdf2_hmac(Algo::Sha256, b"password", b"salt", 1, 40)),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b\
4dbf3a2f3dad3377"
        );
    }

    #[test]
    fn uuid5_namespace_vector() {
        let bytes = uuid5_bytes(&NAMESPACE_DNS, b"python.org");
        assert_eq!(uuid_display(&bytes), "886313e1-3b8a-5372-9b90-0c9aee199e5d");
        assert_eq!(bytes[6] >> 4, 5);
        assert_eq!(bytes[8] >> 6, 2);
    }

    #[test]
    fn uuid_helpers_return_canonical_strings() {
        exc::init();
        let namespace = text("6ba7b810-9dad-11d1-80b4-00c04fd430c8");
        let generated = call("uuid5", &[namespace, text("python.org")]);
        assert!(!exc::pending());
        assert_eq!(
            value::as_str(generated),
            Some("886313e1-3b8a-5372-9b90-0c9aee199e5d")
        );
        let generated = call("uuid4", &[]);
        assert!(!exc::pending());
        let text_form = value::as_str(generated).expect("uuid4 string");
        assert_eq!(text_form.len(), 36);
        assert_eq!(&text_form[14..15], "4");
        assert!(matches!(&text_form[19..20], "8" | "9" | "a" | "b"));
        assert_eq!(value::as_str(call("uuid1", &[])).map(str::len), Some(36));
    }

    #[test]
    fn uuid4_version_and_variant_bits() {
        for _ in 0..4 {
            let bytes = uuid4_bytes().expect("entropy");
            assert_eq!(bytes[6] >> 4, 4);
            assert_eq!(bytes[8] >> 6, 2);
        }
    }

    #[test]
    fn compare_digest_semantics() {
        exc::init();
        let a = bytes(b"secret");
        let b = bytes(b"secret");
        assert!(value::truthy(call("compare_digest", &[a, b])));
        assert!(!value::truthy(call(
            "compare_digest",
            &[a, bytes(b"secreu")]
        )));
        let left = text("abc");
        let right = text("abc");
        assert!(value::truthy(call("compare_digest", &[left, right])));
        let mixed = call("compare_digest", &[a, text("secret")]);
        assert_eq!(clear_error(), TYPE_ERROR);
        assert_eq!(mixed, NONE);
    }

    #[test]
    fn runtime_hash_object_surface() {
        exc::init();
        let hash = call("sha256", &[bytes(b"abc")]);
        assert!(!exc::pending());
        assert_eq!(
            value::as_str(call("hashlib.hexdigest", &[hash])).expect("hexdigest"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            as_int(value::instance_attr(hash, "digest_size").unwrap()),
            32
        );
        assert_eq!(
            as_int(value::instance_attr(hash, "block_size").unwrap()),
            64
        );
        assert_eq!(
            value::as_str(value::instance_attr(hash, "name").unwrap()).unwrap(),
            "sha256"
        );

        let clone = call("hashlib.copy", &[hash]);
        call(
            "hashlib.update",
            &[clone, value::bytearray_new(b"def".to_vec())],
        );
        assert_eq!(
            value::as_str(call("hashlib.hexdigest", &[clone])).unwrap(),
            "bef57ec7f53a6d40beb640a780a639c83bc29ac8a9816f1fc6c5c6dcd93c4721"
        );
        // The original is unaffected by the clone.
        assert_eq!(
            value::as_str(call("hashlib.hexdigest", &[hash])).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn module_keywords_and_errors() {
        exc::init();
        let name = text("blake2b");
        let key = bytes(b"key");
        let data = bytes(b"abc");
        let hash = call_kw(
            "new",
            &[name],
            &[("data".to_string(), data), ("key".to_string(), key)],
        );
        assert!(!exc::pending());
        assert_eq!(
            value::as_str(call("hashlib.hexdigest", &[hash])).unwrap(),
            "5c6a9a4ae911c02fb7e71a991eb9aea371ae993d4842d206e6020d46f5e41358\
c6d5c277c110ef86c959ed63e6ecaaaceaaff38019a43264ae06acf73b9550b1"
        );

        let bad = call("sha256", &[text("not bytes")]);
        assert_eq!(clear_error(), TYPE_ERROR);
        assert_eq!(bad, NONE);

        let bad = call("new", &[text("sha3_256")]);
        assert_eq!(clear_error(), VALUE_ERROR);
        assert_eq!(bad, NONE);

        let bad = call("new", &[bytes(b"sha256")]);
        assert_eq!(clear_error(), TYPE_ERROR);
        assert_eq!(bad, NONE);

        let hash = call_kw("sha256", &[], &[("data".to_string(), bytes(b"abc"))]);
        assert!(!exc::pending());
        assert_eq!(
            value::as_str(call("hashlib.hexdigest", &[hash])).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let name = text("blake2s");
        let bad = call_kw("new", &[name], &[("bogus".to_string(), value::TRUE)]);
        assert_eq!(clear_error(), TYPE_ERROR);
        assert_eq!(bad, NONE);
    }

    #[test]
    fn runtime_blake2_round_trip() {
        exc::init();
        let hash = call("blake2b", &[bytes(b"abc")]);
        assert!(!exc::pending());
        assert_eq!(
            value::as_str(call("hashlib.hexdigest", &[hash])).unwrap(),
            "ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d1\
7d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd4009923"
        );
        let key = bytes(b"key");
        let keyed = call_kw("blake2b", &[bytes(b"abc")], &[("key".to_string(), key)]);
        assert!(!exc::pending());
        assert_eq!(
            value::as_str(call("hashlib.hexdigest", &[keyed])).unwrap(),
            "5c6a9a4ae911c02fb7e71a991eb9aea371ae993d4842d206e6020d46f5e41358\
c6d5c277c110ef86c959ed63e6ecaaaceaaff38019a43264ae06acf73b9550b1"
        );
        let hash = call("blake2s", &[bytes(b"abc")]);
        assert_eq!(
            value::as_str(call("hashlib.hexdigest", &[hash])).unwrap(),
            "508c5e8c327c14e2e1a72ba34eeb452f37458b209ed63a294d999b4c86675982"
        );
    }

    #[test]
    fn hmac_compute_digest_alias() {
        exc::init();
        let digest = call_named(
            "_hmac",
            "compute_digest",
            &[bytes(&[0x0b; 20]), bytes(b"Hi There"), text("sha256")],
        );
        assert!(!exc::pending());
        assert_eq!(
            to_hex(value::as_bytes(digest).unwrap()),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn hmac_runtime_object() {
        exc::init();
        let digest = call(
            "hmac_new",
            &[bytes(&[0x0b; 20]), bytes(b"Hi There"), text("sha256")],
        );
        assert!(!exc::pending());
        assert_eq!(
            value::as_str(value::instance_attr(digest, "name").unwrap()).unwrap(),
            "hmac-sha256"
        );
        assert_eq!(
            value::as_str(call("hashlib.HMAC.hexdigest", &[digest])).unwrap(),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        let copy = call("hashlib.HMAC.copy", &[digest]);
        call("hashlib.HMAC.update", &[copy, bytes(b"x")]);
        assert_eq!(
            value::as_str(call("hashlib.HMAC.hexdigest", &[digest])).unwrap(),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn pbkdf2_runtime_validation() {
        exc::init();
        let bad = call(
            "pbkdf2_hmac",
            &[text("sha256"), bytes(b""), bytes(b""), int_result(0)],
        );
        assert_eq!(clear_error(), VALUE_ERROR);
        assert_eq!(bad, NONE);
        let good = call(
            "pbkdf2_hmac",
            &[
                text("sha256"),
                bytes(b"password"),
                bytes(b"salt"),
                int_result(1),
                int_result(20),
            ],
        );
        assert!(!exc::pending());
        assert_eq!(
            to_hex(value::as_bytes(good).unwrap()),
            "120fb6cffcf8b32c43e7225256c4f837a86548c9"
        );
    }
}
