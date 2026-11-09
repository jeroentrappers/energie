//! A minimal ZIP reader for the EU Data Act datasets: one small JSON file per archive, stored
//! or deflated. Reads the central directory, so archives written with data descriptors work.

use std::io::Read;

use crate::error::{CloudError, Result};

fn u16_at(b: &[u8], at: usize) -> Result<usize> {
    b.get(at..at + 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as usize).ok_or_else(truncated)
}

fn u32_at(b: &[u8], at: usize) -> Result<usize> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize).ok_or_else(truncated)
}

fn truncated() -> CloudError {
    CloudError::Malformed("zip: truncated".into())
}

/// One member: name and contents.
pub fn members(b: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let eocd = (0..b.len().saturating_sub(21))
        .rev()
        .find(|&i| b[i..].starts_with(&[0x50, 0x4b, 0x05, 0x06]))
        .ok_or_else(|| CloudError::Malformed("zip: no end of central directory".into()))?;
    let count = u16_at(b, eocd + 10)?;
    let mut at = u32_at(b, eocd + 16)?;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if !b.get(at..).is_some_and(|s| s.starts_with(&[0x50, 0x4b, 0x01, 0x02])) {
            return Err(CloudError::Malformed("zip: bad central directory".into()));
        }
        let method = u16_at(b, at + 10)?;
        let size = u32_at(b, at + 20)?;
        let (name_len, extra_len, comment_len) = (u16_at(b, at + 28)?, u16_at(b, at + 30)?, u16_at(b, at + 32)?);
        let local = u32_at(b, at + 42)?;
        let name = String::from_utf8_lossy(b.get(at + 46..at + 46 + name_len).ok_or_else(truncated)?).into_owned();
        at += 46 + name_len + extra_len + comment_len;
        let data_at = local + 30 + u16_at(b, local + 26)? + u16_at(b, local + 28)?;
        let raw = b.get(data_at..data_at + size).ok_or_else(truncated)?;
        let data = match method {
            0 => raw.to_vec(),
            8 => {
                let mut v = Vec::new();
                flate2::read::DeflateDecoder::new(raw)
                    .read_to_end(&mut v)
                    .map_err(|e| CloudError::Malformed(format!("zip: {e}")))?;
                v
            }
            m => return Err(CloudError::Malformed(format!("zip: compression method {m}"))),
        };
        out.push((name, data));
    }
    Ok(out)
}

/// The first `.json` member.
pub fn first_json(b: &[u8]) -> Result<Vec<u8>> {
    members(b)?
        .into_iter()
        .find(|(n, _)| n.to_ascii_lowercase().ends_with(".json"))
        .map(|(_, d)| d)
        .ok_or_else(|| CloudError::Malformed("zip: no JSON inside".into()))
}

/// Write a ZIP (tests only): `(name, data, deflate)` per member.
#[cfg(test)]
pub(crate) fn build(files: &[(&str, &[u8], bool)]) -> Vec<u8> {
    use std::io::Write;
    let (mut out, mut cd) = (Vec::new(), Vec::new());
    for (name, data, deflate) in files {
        let body = if *deflate {
            let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(data).unwrap();
            e.finish().unwrap()
        } else {
            data.to_vec()
        };
        let method: u16 = if *deflate { 8 } else { 0 };
        let offset = out.len() as u32;
        let head = |sig: u32, central: bool| {
            let mut h = sig.to_le_bytes().to_vec();
            if central {
                h.extend(20u16.to_le_bytes());
            }
            h.extend(20u16.to_le_bytes());
            h.extend(0u16.to_le_bytes());
            h.extend(method.to_le_bytes());
            h.extend([0u8; 8]); // time, date, crc (not checked)
            h.extend((body.len() as u32).to_le_bytes());
            h.extend((data.len() as u32).to_le_bytes());
            h.extend((name.len() as u16).to_le_bytes());
            h.extend(0u16.to_le_bytes());
            if central {
                h.extend([0u8; 6]); // comment len, disk, internal attrs
                h.extend([0u8; 4]); // external attrs
                h.extend(offset.to_le_bytes());
            }
            h.extend(name.as_bytes());
            h
        };
        out.extend(head(0x0403_4b50, false));
        out.extend(&body);
        cd.extend(head(0x0201_4b50, true));
    }
    let cd_at = out.len() as u32;
    out.extend(&cd);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0u8; 4]);
    out.extend((files.len() as u16).to_le_bytes());
    out.extend((files.len() as u16).to_le_bytes());
    out.extend((cd.len() as u32).to_le_bytes());
    out.extend(cd_at.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_stored_and_deflated_members() {
        let z = build(&[("readme.txt", b"hi", false), ("data.JSON", b"{\"Data\":[]}", true)]);
        let m = members(&z).unwrap();
        assert_eq!(m[0], ("readme.txt".to_string(), b"hi".to_vec()));
        assert_eq!(first_json(&z).unwrap(), b"{\"Data\":[]}");
        assert!(matches!(first_json(&build(&[("a.txt", b"x", false)])), Err(CloudError::Malformed(_))));
    }

    #[test]
    fn rejects_broken_archives() {
        assert!(matches!(members(b"not a zip at all, definitely not"), Err(CloudError::Malformed(_))));
        let z = build(&[("a.json", b"{}", true)]);
        let mut bad_method = z.clone();
        let cd = u32_at(&z, z.len() - 6).unwrap();
        bad_method[cd + 10] = 99;
        assert!(matches!(members(&bad_method), Err(CloudError::Malformed(m)) if m.contains("method 99")));
        let mut bad_cd = z.clone();
        bad_cd[cd] = 0;
        assert!(matches!(members(&bad_cd), Err(CloudError::Malformed(m)) if m.contains("central")));
        let mut bad_size = z.clone();
        bad_size[cd + 20] = 0xff;
        assert!(matches!(members(&bad_size), Err(CloudError::Malformed(m)) if m.contains("truncated")));
        let mut bad_data = z.clone();
        bad_data[30 + 6] = 0xff; // corrupt the deflate stream right after the name
        bad_data[30 + 7] = 0xff;
        assert!(members(&bad_data).is_err());
        assert!(matches!(u16_at(b"x", 0), Err(CloudError::Malformed(_))));
    }
}
