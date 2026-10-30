//! The game's EXE and DLLs: sections at their virtual addresses, imports and exports.

use std::path::Path;

pub struct Section {
    pub name: String,
    pub va: u32,
    pub vsize: u32,
    pub raw: u32,
    pub rsize: u32,
    /// holds code (IMAGE_SCN_CNT_CODE or IMAGE_SCN_MEM_EXECUTE)
    pub exec: bool,
}

pub struct Image {
    pub data: Vec<u8>,
    pub base: u32,
    pub entry: u32,
    pub sections: Vec<Section>,
}

/// One imported function: the IAT slot the game calls through.
pub struct Import {
    pub iat: u32,
    pub dll: String,
    pub name: String,
}

fn le16(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}
fn le32(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

impl Image {
    pub fn load(path: &Path) -> std::io::Result<Image> {
        let data = std::fs::read(path)?;
        let pe = le32(&data, 0x3c) as usize;
        let nsec = le16(&data, pe + 6) as usize;
        let opt = le16(&data, pe + 20) as usize;
        let base = le32(&data, pe + 24 + 28);
        let entry = base + le32(&data, pe + 24 + 16);
        let sections = (0..nsec)
            .map(|i| {
                let o = pe + 24 + opt + i * 40;
                let name = data[o..o + 8].iter().take_while(|&&b| b != 0).map(|&b| b as char).collect();
                Section {
                    name,
                    vsize: le32(&data, o + 8),
                    va: base + le32(&data, o + 12),
                    rsize: le32(&data, o + 16),
                    raw: le32(&data, o + 20),
                    exec: le32(&data, o + 36) & 0x2000_0020 != 0,
                }
            })
            .collect();
        Ok(Image { data, base, entry, sections })
    }

    /// Bytes at a virtual address; zeros past a section's raw data (.bss); `None` outside
    /// every section.
    pub fn read(&self, addr: u32, n: usize) -> Option<Vec<u8>> {
        // the headers, before the first section
        let first = self.sections.iter().map(|s| s.va).min().unwrap_or(self.base);
        if addr >= self.base && addr < first {
            return Some((0..n as u32).map(|k| self.data.get((addr - self.base + k) as usize).copied().unwrap_or(0)).collect());
        }
        let s = self.sections.iter().find(|s| addr >= s.va && addr < s.va + s.vsize.max(s.rsize))?;
        let off = addr - s.va;
        Some(
            (0..n as u32)
                .map(|k| {
                    let o = off + k;
                    if o < s.rsize { self.data.get((s.raw + o) as usize).copied().unwrap_or(0) } else { 0 }
                })
                .collect(),
        )
    }

    /// Move the image to another base: its relocations (.reloc, HIGHLOW entries) applied to
    /// the file's bytes, the sections and the entry moved.
    pub fn rebase(&mut self, base: u32) {
        let delta = base.wrapping_sub(self.base);
        let pe = le32(&self.data, 0x3c) as usize;
        let (rva, size) = (le32(&self.data, pe + 24 + 136), le32(&self.data, pe + 24 + 140));
        // file offset of a virtual address
        let file = |img: &Image, a: u32| {
            let s = img.sections.iter().find(|s| a >= s.va && a < s.va + s.rsize)?;
            Some((s.raw + a - s.va) as usize)
        };
        let mut b = self.base + rva;
        while b < self.base + rva + size {
            let (page, len) = (self.u32(b), self.u32(b + 4));
            if len < 8 {
                break;
            }
            for k in (8..len).step_by(2) {
                let e = self.read(b + k, 2).map_or(0, |e| u16::from_le_bytes([e[0], e[1]]));
                if e >> 12 == 3 {
                    if let Some(o) = file(self, self.base + page + (e & 0xfff) as u32) {
                        let v = le32(&self.data, o).wrapping_add(delta);
                        self.data[o..o + 4].copy_from_slice(&v.to_le_bytes());
                    }
                }
            }
            b += len;
        }
        for s in &mut self.sections {
            s.va = s.va.wrapping_add(delta);
        }
        self.entry = self.entry.wrapping_add(delta);
        self.base = base;
    }

    /// Several images (each at its own base) as one, for finding and compiling their code
    /// together. Base and entry are the first one's; the headers of the others are left out.
    pub fn combine(parts: &[&Image]) -> Image {
        let mut data = Vec::new();
        let mut sections = Vec::new();
        for img in parts {
            let at = data.len() as u32;
            data.extend_from_slice(&img.data);
            sections.extend(img.sections.iter().map(|s| Section { name: s.name.clone(), raw: s.raw + at, ..*s }));
        }
        Image { data, base: parts[0].base, entry: parts[0].entry, sections }
    }

    /// Whether `a` lies in a code section.
    pub fn is_code(&self, a: u32) -> bool {
        self.sections.iter().any(|s| s.exec && a >= s.va && a < s.va + s.vsize.max(s.rsize))
    }

    pub fn byte(&self, addr: u32) -> u8 {
        self.read(addr, 1).map_or(0, |b| b[0])
    }

    pub fn u32(&self, addr: u32) -> u32 {
        self.read(addr, 4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn cstr(&self, addr: u32) -> String {
        (0..).map(|k| self.byte(addr + k)).take_while(|&b| b != 0).map(|b| b as char).collect()
    }

    pub fn imports(&self) -> Vec<Import> {
        let pe = le32(&self.data, 0x3c) as usize;
        let rva = le32(&self.data, pe + 24 + 104);
        let mut out = Vec::new();
        let mut d = self.base + rva;
        loop {
            let (oft, name, ft) = (self.u32(d), self.u32(d + 12), self.u32(d + 16));
            if oft == 0 && name == 0 && ft == 0 {
                break;
            }
            let dll = self.cstr(self.base + name);
            for k in 0.. {
                let thunk = self.u32(self.base + if oft != 0 { oft } else { ft } + 4 * k);
                if thunk == 0 {
                    break;
                }
                let name =
                    if thunk & 0x8000_0000 != 0 { format!("#{}", thunk & 0xffff) } else { self.cstr(self.base + thunk + 2) };
                out.push(Import { iat: self.base + ft + 4 * k, dll: dll.clone(), name });
            }
            d += 20;
        }
        out
    }

    /// The exported functions: (address, name); ordinal-only exports are named `#n`.
    pub fn exports(&self) -> Vec<(u32, String)> {
        let pe = le32(&self.data, 0x3c) as usize;
        let rva = le32(&self.data, pe + 24 + 96);
        if rva == 0 {
            return Vec::new();
        }
        let d = self.base + rva;
        let (ord_base, nfuncs, nnames) = (self.u32(d + 16), self.u32(d + 20), self.u32(d + 24));
        let (funcs, names, ords) = (self.base + self.u32(d + 28), self.base + self.u32(d + 32), self.base + self.u32(d + 36));
        let mut out: Vec<(u32, String)> = (0..nfuncs)
            .filter(|&k| self.u32(funcs + 4 * k) != 0)
            .map(|k| (self.base + self.u32(funcs + 4 * k), format!("#{}", ord_base + k)))
            .collect();
        for k in 0..nnames {
            let ord = (self.u32(ords + 2 * k) & 0xffff) + ord_base;
            let name = self.cstr(self.base + self.u32(names + 4 * k));
            let a = self.base + self.u32(funcs + 4 * (ord - ord_base));
            if let Some(e) = out.iter_mut().find(|e| e.0 == a && e.1.starts_with('#')) {
                e.1 = name;
            }
        }
        out
    }
}

/// The names in data/symbols.txt (what we know about the game's code and data, built into the
/// program), by address.
pub fn symbols() -> &'static std::collections::BTreeMap<u32, String> {
    static S: std::sync::OnceLock<std::collections::BTreeMap<u32, String>> = std::sync::OnceLock::new();
    S.get_or_init(|| {
        include_str!("../data/symbols.txt")
            .lines()
            .filter_map(|l| {
                let mut w = l.split_whitespace();
                let a = u32::from_str_radix(w.next()?.strip_prefix("0x")?, 16).ok()?;
                Some((a, w.next()?.to_string()))
            })
            .collect()
    })
}

/// An address with its name, if it has one: "0x446e00 robot_update".
pub fn named(a: u32) -> String {
    match symbols().get(&a) {
        Some(n) => format!("{a:#x} {n}"),
        None => format!("{a:#x}"),
    }
}

/// A resource name or number (`MAKEINTRESOURCE`).
#[derive(Clone, Copy, Debug)]
pub enum ResId<'a> {
    Id(u32),
    Name(&'a str),
}

impl ResId<'_> {
    /// From a guest argument: a number up to 0xffff, else a pointer to a string.
    pub fn from_guest(p: u32) -> ResId<'static> {
        if p <= 0xffff { ResId::Id(p) } else { ResId::Name(Box::leak(crate::platform::cstr(p).into_boxed_str())) }
    }
}

/// A resource's name: a number or a string.
#[derive(Clone, Debug, PartialEq)]
pub enum ResName {
    Id(u32),
    Name(String),
}

/// The entries of one resource directory level: name and the entry's offset field.
fn res_entries(rd: &dyn Fn(u32) -> u8, root: u32, dir: u32) -> Vec<(ResName, u32)> {
    let r16 = |a: u32| u16::from_le_bytes([rd(a), rd(a + 1)]) as u32;
    let r32 = |a: u32| r16(a) | r16(a + 2) << 16;
    let n = r16(dir + 12) + r16(dir + 14);
    (0..n)
        .map(|k| {
            let e = dir + 16 + 8 * k;
            let name = r32(e);
            let name = if name & 0x8000_0000 != 0 {
                let p = root + (name & 0x7fff_ffff);
                ResName::Name((0..r16(p)).map(|k| char::from_u32(r16(p + 2 + 2 * k)).unwrap_or('?')).collect())
            } else {
                ResName::Id(name)
            };
            (name, r32(e + 4))
        })
        .collect()
}

/// Every resource of a type (first language each): name, address and size of its data.
/// `rd` reads a byte at a virtual address of the image at `base`.
pub fn list_resources(rd: &dyn Fn(u32) -> u8, base: u32, ty: ResId) -> Vec<(ResName, u32, u32)> {
    let r32 = |a: u32| u32::from_le_bytes([rd(a), rd(a + 1), rd(a + 2), rd(a + 3)]);
    let pe = base + r32(base + 0x3c);
    let rva = r32(pe + 24 + 96 + 2 * 8);
    if rva == 0 {
        return Vec::new();
    }
    let root = base + rva;
    let matches = |n: &ResName, want: ResId| match (n, want) {
        (ResName::Id(a), ResId::Id(b)) => *a == b,
        (ResName::Name(a), ResId::Name(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    };
    let sub = |off: u32| (off & 0x8000_0000 != 0).then(|| root + (off & 0x7fff_ffff));
    let Some(types) = res_entries(rd, root, root).into_iter().find(|e| matches(&e.0, ty)).and_then(|e| sub(e.1)) else {
        return Vec::new();
    };
    res_entries(rd, root, types)
        .into_iter()
        .filter_map(|(name, off)| {
            let langs = sub(off)?;
            let leaf = res_entries(rd, root, langs).first()?.1;
            (leaf & 0x8000_0000 == 0).then(|| (name, base + r32(root + leaf), r32(root + leaf + 4)))
        })
        .collect()
}

/// One resource: address and size of its data.
pub fn find_resource(rd: &dyn Fn(u32) -> u8, base: u32, ty: ResId, id: ResId) -> Option<(u32, u32)> {
    list_resources(rd, base, ty)
        .into_iter()
        .find(|(n, ..)| match (n, id) {
            (ResName::Id(a), ResId::Id(b)) => *a == b,
            (ResName::Name(a), ResId::Name(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        })
        .map(|(_, a, n)| (a, n))
}
