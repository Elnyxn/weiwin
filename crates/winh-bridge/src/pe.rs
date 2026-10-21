//! Bounded PE64 import parsing. No instruction scanning or execution.
#![allow(dead_code)]
use crate::hooks;
use std::collections::HashSet;

pub(crate) const CARBON_HASH: &str =
    "b6188de140529f87f7fb8c38e6f43c44ed778f310d069d02332371e2c446433b";
pub(crate) const CORE_HASH: &str =
    "00b8c930a414ab37148beb359adbcecf828ef7e9dd4452dc1dde378a6a5fd3bd";
pub(crate) const CORE_NAME: &str = "Microsoft.CognitiveServices.Speech.core.dll";

#[derive(Clone, Debug)]
pub(crate) struct Import {
    pub name: String,
    pub iat_rva: usize,
    /// Exact on-disk delay thunk, relative to the preferred image base.
    pub delay_stub_rva: Option<usize>,
}
pub(crate) struct Image {
    pub size: usize,
    pub imports: Vec<Import>,
}
fn u16_at(b: &[u8], off: usize) -> Result<u16, &'static str> {
    Ok(u16::from_le_bytes(
        b.get(off..off.checked_add(2).ok_or("overflow")?)
            .ok_or("short PE")?
            .try_into()
            .map_err(|_| "short PE")?,
    ))
}
fn u32_at(b: &[u8], off: usize) -> Result<u32, &'static str> {
    Ok(u32::from_le_bytes(
        b.get(off..off.checked_add(4).ok_or("overflow")?)
            .ok_or("short PE")?
            .try_into()
            .map_err(|_| "short PE")?,
    ))
}
fn u64_at(b: &[u8], off: usize) -> Result<u64, &'static str> {
    Ok(u64::from_le_bytes(
        b.get(off..off.checked_add(8).ok_or("overflow")?)
            .ok_or("short PE")?
            .try_into()
            .map_err(|_| "short PE")?,
    ))
}
struct Section {
    rva: usize,
    raw: usize,
    bytes: usize,
}
struct Reader<'a> {
    b: &'a [u8],
    headers: usize,
    sections: Vec<Section>,
}
impl Reader<'_> {
    fn offset(&self, rva: usize, n: usize) -> Result<usize, &'static str> {
        let off = if rva.checked_add(n).is_some_and(|end| end <= self.headers) {
            rva
        } else {
            let s = self
                .sections
                .iter()
                .find(|s| {
                    rva >= s.rva && rva.checked_add(n).is_some_and(|end| end <= s.rva + s.bytes)
                })
                .ok_or("RVA outside file-backed section")?;
            s.raw + rva - s.rva
        };
        if off.checked_add(n).is_none_or(|end| end > self.b.len()) {
            return Err("RVA outside file");
        }
        Ok(off)
    }
    fn string(&self, rva: usize) -> Result<String, &'static str> {
        let mut bytes = Vec::new();
        for i in 0..512 {
            let byte = self.b[self.offset(rva.checked_add(i).ok_or("overflow")?, 1)?];
            if byte == 0 {
                return String::from_utf8(bytes).map_err(|_| "non-UTF8 import");
            }
            bytes.push(byte);
        }
        Err("unterminated import name")
    }
}

pub(crate) fn imports(bytes: &[u8]) -> Result<Image, &'static str> {
    if u16_at(bytes, 0)? != 0x5a4d {
        return Err("not MZ");
    }
    let nt = u32_at(bytes, 0x3c)? as usize;
    if nt > 1024 * 1024 || u32_at(bytes, nt)? != 0x4550 || u16_at(bytes, nt + 4)? != 0x8664 {
        return Err("not x64 PE");
    }
    let sections = u16_at(bytes, nt + 6)? as usize;
    let opt_size = u16_at(bytes, nt + 20)? as usize;
    let opt = nt + 24;
    if sections == 0 || sections > 96 || opt_size < 224 || u16_at(bytes, opt)? != 0x20b {
        return Err("bad PE64 header");
    }
    let size = u32_at(bytes, opt + 56)? as usize;
    let headers = u32_at(bytes, opt + 60)? as usize;
    if !(4096..=64 * 1024 * 1024).contains(&size) || headers > bytes.len() {
        return Err("bad image size");
    }
    let preferred_base = u64_at(bytes, opt + 24)?;
    let mut reader = Reader {
        b: bytes,
        headers,
        sections: Vec::new(),
    };
    for i in 0..sections {
        let s = opt + opt_size + 40 * i;
        let rva = u32_at(bytes, s + 12)? as usize;
        let count = u32_at(bytes, s + 16)? as usize;
        let raw = u32_at(bytes, s + 20)? as usize;
        if rva.checked_add(count).is_none()
            || raw.checked_add(count).is_none_or(|end| end > bytes.len())
        {
            return Err("bad section bounds");
        }
        reader.sections.push(Section {
            rva,
            raw,
            bytes: count,
        });
    }
    let mut imports = Vec::new();
    let mut found_core = false;
    // Speech.core is a DELAY import in this CarbonDictation build. Parse both
    // directory 1 and 13 instead of mistaking an absent normal entry for failure.
    for (directory_offset, stride, delay) in [(120, 20, false), (216, 32, true)] {
        let dir = u32_at(bytes, opt + directory_offset)? as usize;
        let dir_size = u32_at(bytes, opt + directory_offset + 4)? as usize;
        if dir == 0 && dir_size == 0 {
            continue;
        }
        if dir == 0 || !(stride..=128 * 1024).contains(&dir_size) {
            return Err("bad import directory");
        }
        let mut terminated = false;
        for i in 0..(dir_size / stride).min(1024) {
            let d = reader.offset(dir + stride * i, stride)?;
            if bytes[d..d + stride].iter().all(|v| *v == 0) {
                terminated = true;
                break;
            }
            if delay && u32_at(bytes, d)? != 1 {
                return Err("unsupported non-RVA delay import");
            }
            let name = reader.string(u32_at(bytes, d + if delay { 4 } else { 12 })? as usize)?;
            if !name.eq_ignore_ascii_case(CORE_NAME) {
                continue;
            }
            if found_core {
                return Err("duplicate Speech import descriptor");
            }
            found_core = true;
            let names = u32_at(bytes, d + if delay { 16 } else { 0 })? as usize;
            let slots = u32_at(bytes, d + if delay { 12 } else { 16 })? as usize;
            if names == 0 || slots == 0 || slots % 8 != 0 {
                return Err("missing named import thunk");
            }
            let mut ended = false;
            for j in 0..256 {
                let value = u64_at(bytes, reader.offset(names + j * 8, 8)?)?;
                if value == 0 {
                    ended = true;
                    break;
                }
                if value >> 32 != 0 {
                    return Err("ordinal or invalid Speech import");
                }
                let name = reader.string(value as usize + 2)?;
                let iat_rva = slots + 8 * j;
                if iat_rva.checked_add(8).is_none_or(|end| end > size) {
                    return Err("IAT outside image");
                }
                let delay_stub_rva = if delay {
                    let raw = u64_at(bytes, reader.offset(iat_rva, 8)?)?;
                    let rva = raw
                        .checked_sub(preferred_base)
                        .ok_or("invalid delay thunk")?;
                    if rva >= size as u64 {
                        return Err("delay thunk outside image");
                    }
                    Some(rva as usize)
                } else {
                    None
                };
                imports.push(Import {
                    name,
                    iat_rva,
                    delay_stub_rva,
                });
            }
            if !ended {
                return Err("unterminated Speech thunk");
            }
        }
        if !terminated {
            return Err("unterminated import descriptors");
        }
    }
    if !found_core {
        return Err("missing Speech import directory");
    }
    validate_coverage(&imports)?;
    Ok(Image { size, imports })
}

pub(crate) fn validate_coverage(imports: &[Import]) -> Result<(), &'static str> {
    let mut seen = HashSet::new();
    let mut slots = HashSet::new();
    for import in imports {
        if !seen.insert(import.name.as_str()) || !slots.insert(import.iat_rva) {
            return Err("duplicate Speech import");
        }
        if hooks::is_start_or_stop(&import.name) && hooks::address(&import.name).is_none() {
            return Err("unsupported recognizer start/stop import");
        }
    }
    if hooks::REQUIRED.iter().any(|n| !seen.contains(n)) {
        return Err("missing required Speech import");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    #[test]
    fn malformed_pe_is_rejected_before_pointer_access() {
        for bytes in [vec![], vec![0; 4096], b"MZ".to_vec()] {
            assert!(imports(&bytes).is_err());
        }
    }
    #[test]
    fn missing_or_unknown_start_branch_cannot_partially_install() {
        let mut imports = hooks::REQUIRED
            .iter()
            .enumerate()
            .map(|(i, n)| Import {
                name: n.to_string(),
                iat_rva: i * 8,
                delay_stub_rva: None,
            })
            .collect::<Vec<_>>();
        assert!(validate_coverage(&imports).is_ok());
        imports.push(Import {
            name: "recognizer_start_unknown_mode".into(),
            iat_rva: 9992,
            delay_stub_rva: None,
        });
        assert!(validate_coverage(&imports).is_err());
        imports.pop();
        imports.pop();
        assert!(validate_coverage(&imports).is_err());
    }
    #[test]
    fn explicit_local_carbon_image_matches_verified_import_contract() {
        let Ok(path) = std::env::var("WXB_BRIDGE_TEST_CARBON_PATH") else {
            return;
        };
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(&bytes)), CARBON_HASH);
        let image = imports(&bytes).unwrap();
        assert!(image.imports.iter().all(|i| i.delay_stub_rva.is_some()));
        assert!(
            image
                .imports
                .iter()
                .any(|i| i.name == "result_get_text" && i.iat_rva == 0xe0120)
        );
        assert!(image.imports.iter().any(|i| i.name
            == "recognizer_start_continuous_recognition_async"
            && i.iat_rva == 0xe0230));
    }
}
