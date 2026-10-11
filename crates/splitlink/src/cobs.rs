//! Consistent Overhead Byte Stuffing: the encoded frame contains no zero
//! byte, so a single 0x00 delimits frames and a receiver resynchronizes at
//! the next delimiter after any corruption.

/// Encodes `data` (without the trailing delimiter).
pub fn encode(data: &[u8], out: &mut Vec<u8>) {
    let start = out.len();
    out.push(0); // code byte placeholder
    let mut code_idx = start;
    let mut code: u8 = 1;
    for &b in data {
        if b == 0 {
            out[code_idx] = code;
            code_idx = out.len();
            out.push(0);
            code = 1;
        } else {
            out.push(b);
            code += 1;
            if code == 0xFF {
                out[code_idx] = code;
                code_idx = out.len();
                out.push(0);
                code = 1;
            }
        }
    }
    out[code_idx] = code;
}

/// Decodes one frame (delimiter already removed). `None` if malformed.
pub fn decode(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        let code = data[i] as usize;
        if code == 0 {
            return None;
        }
        let end = i + code;
        if end > data.len() {
            return None;
        }
        let chunk = &data[i + 1..end];
        if chunk.contains(&0) {
            return None;
        }
        out.extend_from_slice(chunk);
        i = end;
        if code < 0xFF && i < data.len() {
            out.push(0);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(d: &[u8]) {
        let mut e = Vec::new();
        encode(d, &mut e);
        assert!(!e.contains(&0), "encoded contains zero: {d:?}");
        assert_eq!(decode(&e).as_deref(), Some(d), "roundtrip {d:?}");
    }

    #[test]
    fn roundtrips() {
        rt(&[]);
        rt(&[0]);
        rt(&[0, 0]);
        rt(&[1, 2, 3]);
        rt(&[1, 0, 2, 0]);
        let long: Vec<u8> = (0..1000u32).map(|i| (i % 255 + 1) as u8).collect();
        rt(&long);
        let mixed: Vec<u8> = (0..1000u32).map(|i| (i % 7) as u8).collect();
        rt(&mixed);
        let ff: Vec<u8> = vec![0xAA; 254];
        rt(&ff);
        let ff2: Vec<u8> = vec![0xAA; 255];
        rt(&ff2);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(&[5, 1, 2]).is_none());
        assert!(decode(&[0]).is_none());
    }
}
