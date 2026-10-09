//! Minimal PE32+ header parsing of a loaded module.

use crate::mem::Memory;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    pub rva: usize,
    pub size: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleInfo {
    pub base: usize,
    pub size: usize,
    pub sections: Vec<Section>,
}

impl ModuleInfo {
    /// Parses the headers of the image mapped at `base`.
    pub fn parse(mem: &dyn Memory, base: usize) -> Option<Self> {
        if mem.read_u16(base)? != 0x5A4D {
            return None;
        }
        let nt = base + mem.read_u32(base + 0x3C)? as usize;
        if mem.read_u32(nt)? != 0x0000_4550 {
            return None;
        }
        let num_sections = mem.read_u16(nt + 6)? as usize;
        let opt_size = mem.read_u16(nt + 20)? as usize;
        let opt = nt + 24;
        if mem.read_u16(opt)? != 0x20B {
            return None; // not PE32+
        }
        let size = mem.read_u32(opt + 56)? as usize;
        let first = opt + opt_size;
        let mut sections = Vec::with_capacity(num_sections);
        for i in 0..num_sections.min(96) {
            let sh = first + i * 40;
            let raw = mem.read_vec(sh, 8)?;
            let end = raw.iter().position(|&b| b == 0).unwrap_or(8);
            let name = String::from_utf8_lossy(&raw[..end]).into_owned();
            let vsize = mem.read_u32(sh + 8)? as usize;
            let rva = mem.read_u32(sh + 12)? as usize;
            sections.push(Section { name, rva, size: vsize });
        }
        Some(Self { base, size, sections })
    }

    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }

    /// `(address, length)` of a section.
    pub fn section_range(&self, name: &str) -> Option<(usize, usize)> {
        self.section(name).map(|s| (self.base + s.rva, s.size))
    }

    pub fn contains(&self, addr: usize) -> bool {
        addr >= self.base && addr < self.base + self.size
    }

    pub fn in_section(&self, name: &str, addr: usize) -> bool {
        self.section_range(name).is_some_and(|(a, l)| addr >= a && addr < a + l)
    }

    pub fn rva(&self, rva: u64) -> usize {
        self.base + rva as usize
    }
}

/// Encodes `s` as UTF-16LE bytes.
pub fn utf16_bytes(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// True if the UTF-16 string `needle` occurs in `hay` (used to find the VersionInfo
/// `OriginalFilename` value inside `.rsrc`).
pub fn contains_utf16(hay: &[u8], needle: &str) -> bool {
    let n = utf16_bytes(needle);
    !n.is_empty() && hay.windows(n.len()).any(|w| w.eq_ignore_ascii_case(&n))
}

/// Base address and file name of a module in the current process: `name` if loaded, else
/// the main executable.
#[cfg(windows)]
pub fn find_module(name: &str) -> Option<(usize, String, bool)> {
    use windows::core::PCWSTR;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: valid NUL-terminated string; GetModuleHandleW does not take a reference.
    if let Ok(h) = unsafe { GetModuleHandleW(PCWSTR(wide.as_ptr())) } {
        return Some((h.0 as usize, name.to_owned(), true));
    }
    // SAFETY: a null name returns the main module handle.
    let h = unsafe { GetModuleHandleW(PCWSTR::null()) }.ok()?;
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
        .unwrap_or_default();
    Some((h.0 as usize, exe, false))
}

#[cfg(not(windows))]
pub fn find_module(_name: &str) -> Option<(usize, String, bool)> {
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mem::SliceMemory;

    /// Builds a fake PE32+ header page with the given `(name, rva, size)` sections.
    pub fn fake_image(base: usize, image_size: usize, sections: &[(&str, usize, usize)]) -> SliceMemory {
        let mut img = vec![0u8; image_size];
        img[0..2].copy_from_slice(&0x5A4Du16.to_le_bytes());
        let nt = 0x80usize;
        img[0x3C..0x40].copy_from_slice(&(nt as u32).to_le_bytes());
        img[nt..nt + 4].copy_from_slice(&0x4550u32.to_le_bytes());
        img[nt + 6..nt + 8].copy_from_slice(&(sections.len() as u16).to_le_bytes());
        let opt_size = 0xF0usize;
        img[nt + 20..nt + 22].copy_from_slice(&(opt_size as u16).to_le_bytes());
        let opt = nt + 24;
        img[opt..opt + 2].copy_from_slice(&0x20Bu16.to_le_bytes());
        img[opt + 56..opt + 60].copy_from_slice(&(image_size as u32).to_le_bytes());
        for (i, (name, rva, size)) in sections.iter().enumerate() {
            let sh = opt + opt_size + i * 40;
            img[sh..sh + name.len()].copy_from_slice(name.as_bytes());
            img[sh + 8..sh + 12].copy_from_slice(&(*size as u32).to_le_bytes());
            img[sh + 12..sh + 16].copy_from_slice(&(*rva as u32).to_le_bytes());
        }
        SliceMemory::new(base, img)
    }

    #[test]
    fn parses_sections() {
        let base = 0x1_4000_0000usize;
        let mem = fake_image(base, 0x4000, &[(".text", 0x1000, 0x1800), (".data", 0x3000, 0x800)]);
        let m = ModuleInfo::parse(&mem, base).unwrap();
        assert_eq!(m.size, 0x4000);
        assert_eq!(m.section_range(".text"), Some((base + 0x1000, 0x1800)));
        assert!(m.in_section(".data", base + 0x3010));
        assert!(!m.in_section(".data", base + 0x1010));
        assert!(m.contains(base + 0x3FFF));
        assert!(!m.contains(base + 0x4000));
        assert!(m.section(".rdata").is_none());
    }

    #[test]
    fn rejects_non_pe() {
        let mem = SliceMemory::new(0x1000, vec![0; 0x200]);
        assert!(ModuleInfo::parse(&mem, 0x1000).is_none());
    }

    #[test]
    fn utf16_search() {
        let mut hay = vec![0u8; 8];
        hay.extend(utf16_bytes("acr-Win64-Shipping.exe"));
        assert!(contains_utf16(&hay, "acr-win64-shipping.exe"));
        assert!(!contains_utf16(&hay, "other.exe"));
    }

    #[cfg(windows)]
    #[test]
    fn parses_own_module() {
        let (base, _, _) = find_module("definitely-not-loaded.dll").unwrap();
        let m = ModuleInfo::parse(&crate::mem::ProcessMemory, base).unwrap();
        assert!(m.section(".text").is_some());
    }
}
