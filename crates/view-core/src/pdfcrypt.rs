//! Opening PDFs that are encrypted without a password to open them: the common kind, which
//! only stops printing or editing. Windows shows those pages, so their text has to be readable
//! too. This is the standard security handler (RC4, AES-128 and AES-256), with the hashes and
//! ciphers it needs written out here. Decryption only: nothing is ever encrypted for keeping,
//! and none of this protects anything, so plain, short implementations are enough.

/// How the document's streams are encrypted.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Method {
    Rc4,
    Aes128,
    Aes256,
    None,
}

pub(crate) struct Crypt {
    key: Vec<u8>,
    method: Method,
}

/// The parts of the encryption dictionary that matter, already read from the file.
pub(crate) struct Params<'a> {
    pub v: u32,
    pub r: u32,
    pub length_bits: u32,
    pub o: &'a [u8],
    pub u: &'a [u8],
    pub ue: &'a [u8],
    pub p: i32,
    pub id0: &'a [u8],
    pub encrypt_metadata: bool,
    /// The stream filter's method (V2, AESV2, AESV3, None), for version 4 and 5.
    pub cfm: Option<&'a [u8]>,
}

const PAD: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08, 0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

impl Crypt {
    /// The key for an empty password, or an error if the file needs a real one.
    pub fn new(p: &Params) -> Result<Crypt, String> {
        let locked = || "This PDF is locked with a password.".to_string();
        let method = match (p.v, p.cfm) {
            (1 | 2, _) => Method::Rc4,
            (4, Some(b"AESV2")) => Method::Aes128,
            (4, Some(b"None")) | (5, Some(b"None")) => Method::None,
            (4, _) => Method::Rc4,
            (5, _) => Method::Aes256,
            _ => return Err("This PDF uses an encryption View can't read.".into()),
        };
        // AES-256 goes with revisions 5 and 6 and their 32-byte key, and only with them.
        if method != Method::None && (method == Method::Aes256) != (p.r >= 5) {
            return Err("This PDF's encryption is damaged.".into());
        }
        if p.r >= 5 {
            if p.u.len() < 48 || p.ue.len() < 32 {
                return Err(locked());
            }
            let hash = |salt: &[u8]| if p.r == 5 { sha256(salt).to_vec() } else { hash_r6(salt) };
            if hash(&p.u[32..40])[..32] != p.u[..32] {
                return Err(locked());
            }
            let ik = hash(&p.u[40..48]);
            let mut key = p.ue[..32].to_vec();
            let aes = Aes::new(&ik[..32]);
            let mut prev = [0u8; 16];
            for block in key.chunks_mut(16) {
                let c: [u8; 16] = block.try_into().unwrap();
                let mut b = c;
                aes.decrypt(&mut b);
                for (x, y) in b.iter_mut().zip(prev) {
                    *x ^= y;
                }
                block.copy_from_slice(&b);
                prev = c;
            }
            return Ok(Crypt { key, method });
        }
        let n = if p.r == 2 { 5 } else { (p.length_bits as usize / 8).clamp(5, 16) };
        let mut buf = PAD.to_vec();
        buf.extend_from_slice(&p.o[..p.o.len().min(32)]);
        buf.extend_from_slice(&p.p.to_le_bytes());
        buf.extend_from_slice(p.id0);
        if p.r >= 4 && !p.encrypt_metadata {
            buf.extend_from_slice(&[0xFF; 4]);
        }
        let mut h = md5(&buf);
        if p.r >= 3 {
            for _ in 0..50 {
                h = md5(&h[..n]);
            }
        }
        let key = h[..n].to_vec();
        // Check the empty password against the file's own check value.
        let ok = if p.r == 2 {
            p.u.len() >= 32 && rc4(&key, &PAD) == p.u[..32]
        } else {
            let mut x = md5(&[&PAD[..], p.id0].concat()).to_vec();
            x = rc4(&key, &x);
            for i in 1..=19u8 {
                let k: Vec<u8> = key.iter().map(|b| b ^ i).collect();
                x = rc4(&k, &x);
            }
            p.u.len() >= 16 && x[..16] == p.u[..16]
        };
        if !ok {
            return Err(locked());
        }
        Ok(Crypt { key, method })
    }

    /// Decrypt a stream of object `num`, generation `gen`.
    pub fn decrypt(&self, num: u32, generation: u16, data: &[u8]) -> Vec<u8> {
        match self.method {
            Method::None => data.to_vec(),
            Method::Aes256 => aes_cbc_decrypt(&self.key, data),
            Method::Rc4 | Method::Aes128 => {
                let mut k = self.key.clone();
                k.extend_from_slice(&num.to_le_bytes()[..3]);
                k.extend_from_slice(&generation.to_le_bytes());
                if self.method == Method::Aes128 {
                    k.extend_from_slice(b"sAlT");
                }
                let h = md5(&k);
                let n = (self.key.len() + 5).min(16);
                if self.method == Method::Rc4 { rc4(&h[..n], data) } else { aes_cbc_decrypt(&h[..16], data) }
            }
        }
    }
}

/// The revision 6 password hash (ISO 32000-2, 7.6.4.3.4), for an empty password.
fn hash_r6(salt: &[u8]) -> Vec<u8> {
    let mut k = sha256(salt).to_vec();
    let mut i = 0u32;
    loop {
        let k1: Vec<u8> = k.repeat(64);
        let aes = Aes::new(&k[..16]);
        let mut e = Vec::with_capacity(k1.len());
        let mut prev: [u8; 16] = k[16..32].try_into().unwrap();
        for block in k1.chunks(16) {
            let mut b: [u8; 16] = block.try_into().unwrap();
            for (x, y) in b.iter_mut().zip(prev) {
                *x ^= y;
            }
            aes.encrypt(&mut b);
            e.extend_from_slice(&b);
            prev = b;
        }
        let sum: u32 = e[..16].iter().map(|&b| b as u32).sum();
        k = match sum % 3 {
            0 => sha256(&e).to_vec(),
            1 => sha512(&e, true),
            _ => sha512(&e, false),
        };
        i += 1;
        if i >= 64 && (*e.last().unwrap() as u32) <= i - 32 {
            break;
        }
    }
    k.truncate(32);
    k
}

fn aes_cbc_decrypt(key: &[u8], data: &[u8]) -> Vec<u8> {
    if data.len() < 32 {
        return Vec::new();
    }
    let aes = Aes::new(key);
    let mut prev: [u8; 16] = data[..16].try_into().unwrap();
    let body = &data[16..data.len() - (data.len() - 16) % 16];
    let mut out = Vec::with_capacity(body.len());
    for block in body.chunks(16) {
        let c: [u8; 16] = block.try_into().unwrap();
        let mut b = c;
        aes.decrypt(&mut b);
        for (x, y) in b.iter_mut().zip(prev) {
            *x ^= y;
        }
        out.extend_from_slice(&b);
        prev = c;
    }
    if let Some(&pad) = out.last() {
        if (1..=16).contains(&pad) && out.len() >= pad as usize && out[out.len() - pad as usize..].iter().all(|&b| b == pad) {
            out.truncate(out.len() - pad as usize);
        }
    }
    out
}

pub(crate) fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut s: [u8; 256] = std::array::from_fn(|i| i as u8);
    let mut j = 0u8;
    for i in 0..256 {
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    let (mut i, mut j) = (0u8, 0u8);
    data.iter()
        .map(|&b| {
            i = i.wrapping_add(1);
            j = j.wrapping_add(s[i as usize]);
            s.swap(i as usize, j as usize);
            b ^ s[s[i as usize].wrapping_add(s[j as usize]) as usize]
        })
        .collect()
}

// ---- Hashes ----

pub(crate) fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 16] = [7, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1, 0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];
    let mut h: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_le_bytes());
    for chunk in msg.chunks(64) {
        let m: Vec<u32> = chunk.chunks(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())).collect();
        let [mut a, mut b, mut c, mut d] = h;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(K[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[(i / 16) * 4 + i % 4]));
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 16];
    for (i, w) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for (x, y) in h.iter_mut().zip(v) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for (i, w) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
    }
    out
}

/// SHA-384 (`short`) or SHA-512.
pub(crate) fn sha512(data: &[u8], short: bool) -> Vec<u8> {
    const K: [u64; 80] = [
        0x428a2f98d728ae22, 0x7137449123ef65cd, 0xb5c0fbcfec4d3b2f, 0xe9b5dba58189dbbc, 0x3956c25bf348b538, 0x59f111f1b605d019, 0x923f82a4af194f9b, 0xab1c5ed5da6d8118, 0xd807aa98a3030242, 0x12835b0145706fbe, 0x243185be4ee4b28c, 0x550c7dc3d5ffb4e2,
        0x72be5d74f27b896f, 0x80deb1fe3b1696b1, 0x9bdc06a725c71235, 0xc19bf174cf692694, 0xe49b69c19ef14ad2, 0xefbe4786384f25e3, 0x0fc19dc68b8cd5b5, 0x240ca1cc77ac9c65, 0x2de92c6f592b0275, 0x4a7484aa6ea6e483, 0x5cb0a9dcbd41fbd4, 0x76f988da831153b5,
        0x983e5152ee66dfab, 0xa831c66d2db43210, 0xb00327c898fb213f, 0xbf597fc7beef0ee4, 0xc6e00bf33da88fc2, 0xd5a79147930aa725, 0x06ca6351e003826f, 0x142929670a0e6e70, 0x27b70a8546d22ffc, 0x2e1b21385c26c926, 0x4d2c6dfc5ac42aed, 0x53380d139d95b3df,
        0x650a73548baf63de, 0x766a0abb3c77b2a8, 0x81c2c92e47edaee6, 0x92722c851482353b, 0xa2bfe8a14cf10364, 0xa81a664bbc423001, 0xc24b8b70d0f89791, 0xc76c51a30654be30, 0xd192e819d6ef5218, 0xd69906245565a910, 0xf40e35855771202a, 0x106aa07032bbd1b8,
        0x19a4c116b8d2d0c8, 0x1e376c085141ab53, 0x2748774cdf8eeb99, 0x34b0bcb5e19b48a8, 0x391c0cb3c5c95a63, 0x4ed8aa4ae3418acb, 0x5b9cca4f7763e373, 0x682e6ff3d6b2b8a3, 0x748f82ee5defb2fc, 0x78a5636f43172f60, 0x84c87814a1f0ab72, 0x8cc702081a6439ec,
        0x90befffa23631e28, 0xa4506cebde82bde9, 0xbef9a3f7b2c67915, 0xc67178f2e372532b, 0xca273eceea26619c, 0xd186b8c721c0c207, 0xeada7dd6cde0eb1e, 0xf57d4f7fee6ed178, 0x06f067aa72176fba, 0x0a637dc5a2c898a6, 0x113f9804bef90dae, 0x1b710b35131c471b,
        0x28db77f523047d84, 0x32caab7b40c72493, 0x3c9ebe0a15c9bebc, 0x431d67c49c100d4c, 0x4cc5d4becb3e42b6, 0x597f299cfc657e2a, 0x5fcb6fab3ad6faec, 0x6c44198c4a475817,
    ];
    let mut h: [u64; 8] = if short {
        [0xcbbb9d5dc1059ed8, 0x629a292a367cd507, 0x9159015a3070dd17, 0x152fecd8f70e5939, 0x67332667ffc00b31, 0x8eb44a8768581511, 0xdb0c2e0d64f98fa7, 0x47b5481dbefa4fa4]
    } else {
        [0x6a09e667f3bcc908, 0xbb67ae8584caa73b, 0x3c6ef372fe94f82b, 0xa54ff53a5f1d36f1, 0x510e527fade682d1, 0x9b05688c2b3e6c1f, 0x1f83d9abfb41bd6b, 0x5be0cd19137e2179]
    };
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 128 != 112 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u128).wrapping_mul(8)).to_be_bytes());
    for chunk in msg.chunks(128) {
        let mut w = [0u64; 80];
        for i in 0..16 {
            w[i] = u64::from_be_bytes(chunk[i * 8..i * 8 + 8].try_into().unwrap());
        }
        for i in 16..80 {
            let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
            let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..80 {
            let s1 = v[4].rotate_right(14) ^ v[4].rotate_right(18) ^ v[4].rotate_right(41);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(28) ^ v[0].rotate_right(34) ^ v[0].rotate_right(39);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for (x, y) in h.iter_mut().zip(v) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out: Vec<u8> = h.iter().flat_map(|w| w.to_be_bytes()).collect();
    out.truncate(if short { 48 } else { 64 });
    out
}

// ---- AES ----

pub(crate) struct Aes {
    rounds: usize,
    keys: [u8; 240],
    sbox: [u8; 256],
    inv: [u8; 256],
}

fn xtime(b: u8) -> u8 {
    (b << 1) ^ if b & 0x80 != 0 { 0x1B } else { 0 }
}

fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        a = xtime(a);
        b >>= 1;
    }
    p
}

impl Aes {
    /// A 16- or 32-byte key.
    pub fn new(key: &[u8]) -> Aes {
        // The S-box, worked out rather than typed in: each byte's inverse in GF(2^8), then the
        // affine transform.
        let mut sbox = [0u8; 256];
        let (mut p, mut q) = (1u8, 1u8);
        loop {
            p = p ^ (p << 1) ^ if p & 0x80 != 0 { 0x1B } else { 0 };
            q ^= q << 1;
            q ^= q << 2;
            q ^= q << 4;
            if q & 0x80 != 0 {
                q ^= 0x09;
            }
            sbox[p as usize] = q ^ q.rotate_left(1) ^ q.rotate_left(2) ^ q.rotate_left(3) ^ q.rotate_left(4) ^ 0x63;
            if p == 1 {
                break;
            }
        }
        sbox[0] = 0x63;
        let mut inv = [0u8; 256];
        for (i, &s) in sbox.iter().enumerate() {
            inv[s as usize] = i as u8;
        }
        let nk = if key.len() >= 32 { 8 } else { 4 };
        let rounds = nk + 6;
        let mut keys = [0u8; 240];
        keys[..nk * 4].copy_from_slice(&key[..nk * 4]);
        let mut rcon = 1u8;
        for i in nk..4 * (rounds + 1) {
            let mut t: [u8; 4] = keys[(i - 1) * 4..i * 4].try_into().unwrap();
            if i % nk == 0 {
                t = [sbox[t[1] as usize] ^ rcon, sbox[t[2] as usize], sbox[t[3] as usize], sbox[t[0] as usize]];
                rcon = xtime(rcon);
            } else if nk > 6 && i % nk == 4 {
                t = t.map(|b| sbox[b as usize]);
            }
            for k in 0..4 {
                keys[i * 4 + k] = keys[(i - nk) * 4 + k] ^ t[k];
            }
        }
        Aes { rounds, keys, sbox, inv }
    }

    fn add(&self, s: &mut [u8; 16], round: usize) {
        for (x, k) in s.iter_mut().zip(&self.keys[round * 16..round * 16 + 16]) {
            *x ^= k;
        }
    }

    pub fn encrypt(&self, s: &mut [u8; 16]) {
        self.add(s, 0);
        for round in 1..=self.rounds {
            for x in s.iter_mut() {
                *x = self.sbox[*x as usize];
            }
            let t = *s;
            for c in 0..4 {
                for r in 0..4 {
                    s[r + 4 * c] = t[r + 4 * ((c + r) % 4)];
                }
            }
            if round != self.rounds {
                for c in 0..4 {
                    let a: [u8; 4] = s[c * 4..c * 4 + 4].try_into().unwrap();
                    for r in 0..4 {
                        s[c * 4 + r] = xtime(a[r]) ^ (xtime(a[(r + 1) % 4]) ^ a[(r + 1) % 4]) ^ a[(r + 2) % 4] ^ a[(r + 3) % 4];
                    }
                }
            }
            self.add(s, round);
        }
    }

    pub fn decrypt(&self, s: &mut [u8; 16]) {
        self.add(s, self.rounds);
        for round in (0..self.rounds).rev() {
            let t = *s;
            for c in 0..4 {
                for r in 0..4 {
                    s[r + 4 * ((c + r) % 4)] = t[r + 4 * c];
                }
            }
            for x in s.iter_mut() {
                *x = self.inv[*x as usize];
            }
            self.add(s, round);
            if round != 0 {
                for c in 0..4 {
                    let a: [u8; 4] = s[c * 4..c * 4 + 4].try_into().unwrap();
                    for r in 0..4 {
                        s[c * 4 + r] = gmul(a[r], 14) ^ gmul(a[(r + 1) % 4], 11) ^ gmul(a[(r + 2) % 4], 13) ^ gmul(a[(r + 3) % 4], 9);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn aes_256_needs_its_own_revisions() {
        // An empty password that checks out for revision 3 with a 5-byte key...
        let (o, id0): ([u8; 32], &[u8]) = ([7u8; 32], b"id");
        let mut h = md5(&[&PAD[..], &o[..], &(-4i32).to_le_bytes()[..], id0].concat());
        for _ in 0..50 {
            h = md5(&h[..5]);
        }
        let key = &h[..5];
        let mut x = rc4(key, &md5(&[&PAD[..], id0].concat()));
        for i in 1..=19u8 {
            x = rc4(&key.iter().map(|b| b ^ i).collect::<Vec<_>>(), &x);
        }
        let u3 = [&x[..16], &[0u8; 16][..]].concat();
        let r3 = |v, cfm| Params { v, r: 3, length_bits: 40, o: &o, u: &u3, ue: &[], p: -4, id0, encrypt_metadata: true, cfm };
        assert!(Crypt::new(&r3(2, None)).is_ok());
        // ...isn't let through as AES-256, whose key that short would be.
        assert!(Crypt::new(&r3(5, Some(b"AESV3"))).is_err());
        // And one that checks out for revision 6 isn't let through as RC4.
        let u6 = [&hash_r6(&[1; 8])[..32], &[1u8; 8][..], &[2u8; 8][..]].concat();
        let r6 = |v, cfm| Params { v, r: 6, length_bits: 256, o: &o, u: &u6, ue: &[0; 32], p: -4, id0, encrypt_metadata: true, cfm };
        assert!(Crypt::new(&r6(5, Some(b"AESV3"))).is_ok());
        assert!(Crypt::new(&r6(2, None)).is_err());
    }

    #[test]
    fn hashes_match_their_test_vectors() {
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hex(&sha512(b"abc", false)), "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f");
        assert_eq!(hex(&sha512(b"abc", true)), "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7");
        let long = vec![b'a'; 1_000_000];
        assert_eq!(hex(&sha256(&long)), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    }

    #[test]
    fn ciphers_match_their_test_vectors() {
        assert_eq!(hex(&rc4(b"Key", b"Plaintext")), "bbf316e8d940af0ad3");
        let pt: [u8; 16] = std::array::from_fn(|i| (i as u8) * 0x11);
        for (len, want) in [(16, "69c4e0d86a7b0430d8cdb78070b4c55a"), (32, "8ea2b7ca516745bfeafc49904b496089")] {
            let key: Vec<u8> = (0..len as u8).collect();
            let aes = Aes::new(&key);
            let mut b = pt;
            aes.encrypt(&mut b);
            assert_eq!(hex(&b), want);
            aes.decrypt(&mut b);
            assert_eq!(b, pt);
        }
    }
}
