//! Pure-Rust PDF Standard Security Handler (V=2, R=3, 128-bit RC4) for
//! encrypting PDFs on save and checking encryption status.

use std::collections::BTreeMap;

use crate::document::DocumentError;

const PADDING: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41,
    0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08,
    0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80,
    0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

/// Options for encrypting a PDF document on save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptionOptions {
    /// Password required to open the PDF.
    pub user_password: String,
    /// Optional owner/permissions password (defaults to `user_password` if empty).
    pub owner_password: String,
    pub allow_print: bool,
    pub allow_copy: bool,
    pub allow_modify: bool,
    pub allow_annotations: bool,
}

impl Default for EncryptionOptions {
    fn default() -> Self {
        Self {
            user_password: String::new(),
            owner_password: String::new(),
            allow_print: true,
            allow_copy: true,
            allow_modify: true,
            allow_annotations: true,
        }
    }
}

impl EncryptionOptions {
    /// Compute the 32-bit PDF `/P` permissions integer (ISO 32000-1 Table 22).
    pub fn permissions_i32(&self) -> i32 {
        // Base bits: bits 1-2 = 0, bits 7-8 = 1, bits 13-32 = 1 => 0xFFFF_F0C0
        let mut p: u32 = 0xFFFF_F0C0;
        if self.allow_print {
            p |= (1 << 2) | (1 << 11); // bit 3 (print) + bit 12 (high-res print)
        }
        if self.allow_modify {
            p |= (1 << 3) | (1 << 10); // bit 4 (modify) + bit 11 (assemble)
        }
        if self.allow_copy {
            p |= (1 << 4) | (1 << 9); // bit 5 (copy) + bit 10 (accessibility extract)
        }
        if self.allow_annotations {
            p |= (1 << 5) | (1 << 8); // bit 6 (annots/forms) + bit 9 (fill forms)
        }
        p as i32
    }
}

/// Minimal pure-Rust MD5 (RFC 1321).
fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22,
        5,  9, 14, 20, 5,  9, 14, 20, 5,  9, 14, 20, 5,  9, 14, 20,
        4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
        6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee,
        0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
        0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be,
        0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
        0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa,
        0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
        0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
        0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c,
        0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
        0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05,
        0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
        0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039,
        0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1,
        0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
    ];

    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    let mut a0: u32 = 0x67452301;
    let mut b0: u32 = 0xefcdab89;
    let mut c0: u32 = 0x98badcfe;
    let mut d0: u32 = 0x10325476;

    for chunk in msg.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (i, w) in m.iter_mut().enumerate() {
            *w = u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | ((!b) & d), i),
                16..=31 => ((d & b) | ((!d) & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | (!d)), (7 * i) % 16),
            };
            let temp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(K[i])
                    .wrapping_add(m[g])
                    .rotate_left(S[i]),
            );
            a = temp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

/// RC4 stream cipher.
fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut s = [0u8; 256];
    for (i, v) in s.iter_mut().enumerate() {
        *v = i as u8;
    }
    let mut j: u8 = 0;
    for i in 0..256 {
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    let mut out = Vec::with_capacity(data.len());
    let (mut i, mut j) = (0u8, 0u8);
    for &b in data {
        i = i.wrapping_add(1);
        j = j.wrapping_add(s[i as usize]);
        s.swap(i as usize, j as usize);
        let k = s[(s[i as usize].wrapping_add(s[j as usize])) as usize];
        out.push(b ^ k);
    }
    out
}

fn pad_password(pw: &str) -> [u8; 32] {
    let bytes = pw.as_bytes();
    let mut out = [0u8; 32];
    let n = bytes.len().min(32);
    out[..n].copy_from_slice(&bytes[..n]);
    if n < 32 {
        out[n..].copy_from_slice(&PADDING[..32 - n]);
    }
    out
}

fn compute_owner_key(user_pw: &str, owner_pw: &str) -> [u8; 32] {
    let owner_src = if owner_pw.is_empty() { user_pw } else { owner_pw };
    let padded_owner = pad_password(owner_src);
    let mut hash = md5(&padded_owner);
    for _ in 0..50 {
        hash = md5(&hash);
    }
    let padded_user = pad_password(user_pw);
    let mut buf = rc4(&hash, &padded_user);
    for i in 1u8..=19 {
        let mut key_i = hash;
        for b in &mut key_i {
            *b ^= i;
        }
        buf = rc4(&key_i, &buf);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&buf[..32]);
    out
}

fn compute_encryption_key(user_pw: &str, o_key: &[u8; 32], p: i32, file_id: &[u8; 16]) -> [u8; 16] {
    let padded_user = pad_password(user_pw);
    let mut buf = Vec::with_capacity(32 + 32 + 4 + 16);
    buf.extend_from_slice(&padded_user);
    buf.extend_from_slice(o_key);
    buf.extend_from_slice(&p.to_le_bytes());
    buf.extend_from_slice(file_id);
    let mut hash = md5(&buf);
    for _ in 0..50 {
        hash = md5(&hash);
    }
    hash
}

fn compute_user_key(enc_key: &[u8; 16], file_id: &[u8; 16]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(48);
    buf.extend_from_slice(&PADDING);
    buf.extend_from_slice(file_id);
    let hash = md5(&buf);
    let mut enc = rc4(enc_key, &hash);
    for i in 1u8..=19 {
        let mut key_i = *enc_key;
        for b in &mut key_i {
            *b ^= i;
        }
        enc = rc4(&key_i, &enc);
    }
    let mut out = [0u8; 32];
    out[..16].copy_from_slice(&enc[..16]);
    out
}

fn object_key(enc_key: &[u8; 16], obj_num: u32, gen_num: u16) -> [u8; 16] {
    let mut input = [0u8; 21];
    input[..16].copy_from_slice(enc_key);
    let obj_bytes = obj_num.to_le_bytes();
    input[16] = obj_bytes[0];
    input[17] = obj_bytes[1];
    input[18] = obj_bytes[2];
    let gen_bytes = gen_num.to_le_bytes();
    input[19] = gen_bytes[0];
    input[20] = gen_bytes[1];
    md5(&input)
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0F) as usize] as char);
    }
    s
}

fn find_last_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Parse all `xref` sections (following `/Prev`) to collect the newest byte
/// offset for every in-use object `(obj_num -> (offset, gen_num))` plus the
/// latest `/Root` and `/Info` references.
fn collect_xref_objects(
    bytes: &[u8],
) -> Result<(BTreeMap<u32, (usize, u16)>, String, Option<String>), DocumentError> {
    let sx_pos = find_last_subslice(bytes, b"startxref")
        .ok_or_else(|| DocumentError("missing startxref in PDF".into()))?;
    let after_sx = std::str::from_utf8(&bytes[sx_pos + 9..])
        .map_err(|_| DocumentError("invalid UTF-8 after startxref".into()))?;
    let mut xref_offset: Option<usize> = after_sx
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok());

    let mut objects: BTreeMap<u32, (usize, u16)> = BTreeMap::new();
    let mut root_ref: Option<String> = None;
    let mut info_ref: Option<String> = None;
    let mut visited = std::collections::HashSet::new();

    while let Some(pos) = xref_offset {
        if !visited.insert(pos) || pos >= bytes.len() {
            break;
        }
        let section = &bytes[pos..];
        let tr_rel = find_subslice(section, b"trailer")
            .ok_or_else(|| DocumentError("missing trailer after xref".into()))?;
        let xref_str = String::from_utf8_lossy(&section[..tr_rel]);
        let mut lines = xref_str.lines();
        if let Some(first) = lines.next()
            && !first.trim().starts_with("xref")
        {
            return Err(DocumentError("unsupported cross-reference stream".into()));
        }

        while let Some(header) = lines.next() {
            let parts: Vec<&str> = header.split_whitespace().collect();
            if parts.len() != 2 {
                continue;
            }
            let Ok(start_id) = parts[0].parse::<u32>() else {
                continue;
            };
            let Ok(count) = parts[1].parse::<u32>() else {
                continue;
            };
            for i in 0..count {
                let Some(entry) = lines.next() else {
                    break;
                };
                let cols: Vec<&str> = entry.split_whitespace().collect();
                if cols.len() >= 3 && cols[2] == "n" {
                    if let (Ok(off), Ok(gen_num)) =
                        (cols[0].parse::<usize>(), cols[1].parse::<u16>())
                    {
                        let obj_id = start_id + i;
                        objects.entry(obj_id).or_insert((off, gen_num));
                    }
                }
            }
        }

        let tr_end_rel = find_subslice(&section[tr_rel..], b"startxref")
            .unwrap_or(section.len() - tr_rel);
        let trailer_str = String::from_utf8_lossy(&section[tr_rel..tr_rel + tr_end_rel]);

        if root_ref.is_none()
            && let Some(after_root) = trailer_str.split("/Root").nth(1)
        {
            let toks: Vec<&str> = after_root.split_whitespace().take(3).collect();
            if toks.len() >= 3 && toks[2].starts_with('R') {
                root_ref = Some(format!("{} {} R", toks[0], toks[1]));
            }
        }
        if info_ref.is_none()
            && let Some(after_info) = trailer_str.split("/Info").nth(1)
        {
            let toks: Vec<&str> = after_info.split_whitespace().take(3).collect();
            if toks.len() >= 3 && toks[2].starts_with('R') {
                info_ref = Some(format!("{} {} R", toks[0], toks[1]));
            }
        }

        xref_offset = trailer_str
            .split("/Prev")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse::<usize>().ok());
    }

    let root = root_ref.ok_or_else(|| DocumentError("missing /Root in PDF trailer".into()))?;
    Ok((objects, root, info_ref))
}

/// Parse a PDF literal string starting at `idx` (pointing at `'('`), returning
/// `(decoded_bytes, next_index_after_closing_paren)`.
fn parse_literal_string(body: &[u8], start: usize) -> (Vec<u8>, usize) {
    let mut out = Vec::new();
    let mut i = start + 1;
    let mut depth = 1usize;
    while i < body.len() {
        let b = body[i];
        if b == b'\\' && i + 1 < body.len() {
            i += 1;
            match body[i] {
                b'n' => out.push(b'\n'),
                b'r' => out.push(b'\r'),
                b't' => out.push(b'\t'),
                b'b' => out.push(0x08),
                b'f' => out.push(0x0C),
                b'(' => out.push(b'('),
                b')' => out.push(b')'),
                b'\\' => out.push(b'\\'),
                b'\r' => {
                    if i + 1 < body.len() && body[i + 1] == b'\n' {
                        i += 1;
                    }
                }
                b'\n' => {}
                c if (b'0'..=b'7').contains(&c) => {
                    let mut val = (c - b'0') as u8;
                    for _ in 0..2 {
                        if i + 1 < body.len() && (b'0'..=b'7').contains(&body[i + 1]) {
                            i += 1;
                            val = val.wrapping_mul(8).wrapping_add(body[i] - b'0');
                        } else {
                            break;
                        }
                    }
                    out.push(val);
                }
                other => out.push(other),
            }
            i += 1;
        } else if b == b'(' {
            depth += 1;
            out.push(b);
            i += 1;
        } else if b == b')' {
            depth -= 1;
            i += 1;
            if depth == 0 {
                break;
            }
            out.push(b);
        } else {
            out.push(b);
            i += 1;
        }
    }
    (out, i)
}

/// Parse a PDF hex string starting at `start` (pointing at `'<'`, not `'<<'`),
/// returning `(decoded_bytes, next_index_after_closing_gt)`.
fn parse_hex_string(body: &[u8], start: usize) -> (Vec<u8>, usize) {
    let mut hex = Vec::new();
    let mut i = start + 1;
    while i < body.len() {
        let b = body[i];
        i += 1;
        if b == b'>' {
            break;
        }
        if b.is_ascii_hexdigit() {
            hex.push(b);
        }
    }
    if hex.len() % 2 == 1 {
        hex.push(b'0');
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for pair in hex.chunks_exact(2) {
        let s = std::str::from_utf8(pair).unwrap_or("00");
        out.push(u8::from_str_radix(s, 16).unwrap_or(0));
    }
    (out, i)
}

/// Parse direct `/Length N` from a dictionary slice if present (ignoring
/// indirect `/Length N 0 R`).
fn direct_stream_length(dict_bytes: &[u8]) -> Option<usize> {
    let s = String::from_utf8_lossy(dict_bytes);
    let pos = s.rfind("/Length")?;
    let after = s[pos + 7..].trim_start();
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = after[digits.len()..].trim_start();
    // If followed by `0 R` (generation + R), it's an indirect reference.
    if let Some(after_gen) = rest.strip_prefix(|c: char| c.is_ascii_digit()) {
        if after_gen.trim_start().starts_with('R') {
            return None;
        }
    }
    digits.parse().ok()
}

/// Encrypt all strings and stream data inside a single indirect object body.
fn encrypt_object_body(body: &[u8], obj_key: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0usize;
    while i < body.len() {
        let b = body[i];
        if b == b'%' {
            // Comment until end of line
            while i < body.len() && body[i] != b'\n' && body[i] != b'\r' {
                out.push(body[i]);
                i += 1;
            }
        } else if b == b'(' {
            let (raw, next_i) = parse_literal_string(body, i);
            let enc = rc4(obj_key, &raw);
            out.push(b'<');
            out.extend_from_slice(to_hex(&enc).as_bytes());
            out.push(b'>');
            i = next_i;
        } else if b == b'<' {
            if i + 1 < body.len() && body[i + 1] == b'<' {
                out.extend_from_slice(b"<<");
                i += 2;
            } else {
                let (raw, next_i) = parse_hex_string(body, i);
                let enc = rc4(obj_key, &raw);
                out.push(b'<');
                out.extend_from_slice(to_hex(&enc).as_bytes());
                out.push(b'>');
                i = next_i;
            }
        } else if body[i..].starts_with(b"stream")
            && (i == 0 || body[i - 1].is_ascii_whitespace() || body[i - 1] == b'>')
            && (body.get(i + 6).copied() == Some(b'\n')
                || (body.get(i + 6).copied() == Some(b'\r')
                    && body.get(i + 7).copied() == Some(b'\n')))
        {
            let nl_len = if body.get(i + 6).copied() == Some(b'\r') {
                2
            } else {
                1
            };
            let data_start = i + 6 + nl_len;
            let data_end = if let Some(len) = direct_stream_length(&body[..i])
                && data_start + len <= body.len()
            {
                data_start + len
            } else if let Some(rel_end) = find_subslice(&body[data_start..], b"endstream") {
                let mut end = data_start + rel_end;
                if end > data_start && body[end - 1] == b'\n' {
                    end -= 1;
                }
                if end > data_start && body[end - 1] == b'\r' {
                    end -= 1;
                }
                end
            } else {
                body.len()
            };

            out.extend_from_slice(&body[i..data_start]);
            let enc = rc4(obj_key, &body[data_start..data_end]);
            out.extend_from_slice(&enc);
            if let Some(endstream_rel) = find_subslice(&body[data_end..], b"endstream") {
                out.extend_from_slice(&body[data_end..data_end + endstream_rel + 9]);
                i = data_end + endstream_rel + 9;
            } else {
                i = data_end;
            }
        } else {
            out.push(b);
            i += 1;
        }
    }
    out
}

/// Encrypt a serialized PDF byte buffer using Standard Security Handler
/// (V=2, R=3, 128-bit RC4).
pub fn encrypt_pdf_bytes(bytes: &[u8], opts: &EncryptionOptions) -> Result<Vec<u8>, DocumentError> {
    if opts.user_password.is_empty() && opts.owner_password.is_empty() {
        return Err(DocumentError("password cannot be empty".into()));
    }

    let (xref_objs, root_ref, info_ref) = collect_xref_objects(bytes)?;

    // Deterministic 16-byte File ID derived from content hash.
    let file_id = md5(&bytes[..bytes.len().min(4096)]);
    let p = opts.permissions_i32();
    let o_key = compute_owner_key(&opts.user_password, &opts.owner_password);
    let enc_key = compute_encryption_key(&opts.user_password, &o_key, p, &file_id);
    let u_key = compute_user_key(&enc_key, &file_id);

    let max_id = xref_objs.keys().copied().max().unwrap_or(0);
    let encrypt_id = max_id + 1;
    let total_size = encrypt_id + 1;

    let mut out = Vec::with_capacity(bytes.len() + 1024);
    out.extend_from_slice(b"%PDF-1.5\n%\xE2\xE3\xCF\xD3\n");

    let mut new_offsets: BTreeMap<u32, (usize, u16)> = BTreeMap::new();
    for (&obj_id, &(off, gen_num)) in &xref_objs {
        if obj_id == 0 || off >= bytes.len() {
            continue;
        }
        let slice = &bytes[off..];
        let Some(obj_kw) = find_subslice(slice, b"obj") else {
            continue;
        };
        let body_start = off + obj_kw + 3;
        let Some(endobj_rel) = find_subslice(&bytes[body_start..], b"endobj") else {
            continue;
        };
        let raw_body = &bytes[body_start..body_start + endobj_rel];
        let obj_key = object_key(&enc_key, obj_id, gen_num);
        let encrypted_body = encrypt_object_body(raw_body, &obj_key);

        let new_off = out.len();
        new_offsets.insert(obj_id, (new_off, gen_num));
        out.extend_from_slice(format!("{obj_id} {gen_num} obj").as_bytes());
        out.extend_from_slice(&encrypted_body);
        if !encrypted_body.ends_with(b"\n") {
            out.push(b'\n');
        }
        out.extend_from_slice(b"endobj\n");
    }

    let encrypt_off = out.len();
    new_offsets.insert(encrypt_id, (encrypt_off, 0));
    let encrypt_obj = format!(
        "{encrypt_id} 0 obj\n<< /Filter /Standard /V 2 /R 3 /Length 128 /P {p} /O <{}> /U <{}> >>\nendobj\n",
        to_hex(&o_key),
        to_hex(&u_key),
    );
    out.extend_from_slice(encrypt_obj.as_bytes());

    let xref_off = out.len();
    out.extend_from_slice(format!("xref\n0 {total_size}\n").as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for id in 1..total_size {
        if let Some(&(off, gen_num)) = new_offsets.get(&id) {
            out.extend_from_slice(format!("{off:010} {gen_num:05} n \n").as_bytes());
        } else {
            out.extend_from_slice(b"0000000000 65535 f \n");
        }
    }

    let id_hex = to_hex(&file_id);
    let info_part = info_ref
        .map(|r| format!(" /Info {r}"))
        .unwrap_or_default();
    let trailer = format!(
        "trailer\n<< /Size {total_size} /Root {root_ref}{info_part} /Encrypt {encrypt_id} 0 R /ID [<{id_hex}> <{id_hex}>] >>\nstartxref\n{xref_off}\n%%EOF\n"
    );
    out.extend_from_slice(trailer.as_bytes());

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_and_rc4_match_vectors() {
        assert_eq!(to_hex(&md5(b"")).to_lowercase(), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(to_hex(&md5(b"abc")).to_lowercase(), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(to_hex(&rc4(b"Key", b"Plaintext")), "BBF316E8D940AF0AD3");
    }
}
