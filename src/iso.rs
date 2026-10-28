//! Reading files from a CD image: ISO 9660 with Joliet names when there are, in an `.iso` (2048
//! bytes a sector) or a raw `.bin` (2352 bytes a sector, mode 1 or mode 2 form 1).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// A file or folder on the image.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    sector: u32,
    pub size: u32,
}

pub struct Iso {
    /// bytes copied so far, and who hears of them after each file
    pub copied: u64,
    pub progress: Option<Box<dyn FnMut(u64)>>,
    file: File,
    /// bytes a sector in the file, and where its 2048 data bytes start
    sector_size: u64,
    data_at: u64,
    joliet: bool,
    root: Entry,
}

/// The layouts an image can have: (sector size, data offset).
const LAYOUTS: [(u64, u64); 3] = [(2048, 0), (2352, 16), (2352, 24)];

impl Iso {
    /// Open a CD image; None when the file is none.
    pub fn open(path: &Path) -> Option<Iso> {
        let file = File::open(path).ok()?;
        for (sector_size, data_at) in LAYOUTS {
            let mut iso = Iso {
                copied: 0,
                progress: None,
                file: file.try_clone().ok()?,
                sector_size,
                data_at,
                joliet: false,
                root: Entry::default(),
            };
            if iso.descriptors().is_some() {
                return Some(iso);
            }
        }
        None
    }

    /// The volume descriptors from sector 16: the root folder of the primary one, or of the
    /// Joliet one (UCS-2 names) when there is one.
    fn descriptors(&mut self) -> Option<()> {
        let mut found = false;
        for s in 16..64 {
            let d = self.sector(s).ok()?;
            if &d[1..6] != b"CD001" {
                return None;
            }
            match d[0] {
                1 if !found => {
                    self.root = record(&d[156..], false)?;
                    found = true;
                }
                // Joliet: a supplementary descriptor with the escape sequence %/@, %/C or %/E
                2 if d[88] == b'%' && d[89] == b'/' && b"@CE".contains(&d[90]) => {
                    self.root = record(&d[156..], true)?;
                    self.joliet = true;
                }
                0xff => break,
                _ => {}
            }
        }
        found.then_some(())
    }

    fn sector(&mut self, n: u32) -> std::io::Result<[u8; 2048]> {
        let mut b = [0u8; 2048];
        self.file.seek(SeekFrom::Start(n as u64 * self.sector_size + self.data_at))?;
        self.file.read_exact(&mut b)?;
        Ok(b)
    }

    pub fn root(&self) -> Entry {
        self.root.clone()
    }

    /// The entries of a folder.
    pub fn list(&mut self, dir: &Entry) -> std::io::Result<Vec<Entry>> {
        let mut out = Vec::new();
        for k in 0..dir.size.div_ceil(2048) {
            let d = self.sector(dir.sector + k)?;
            let mut at = 0;
            // records do not cross sectors; a length of 0 ends the sector's
            while at < 2048 && d[at] != 0 {
                let len = d[at] as usize;
                if let Some(e) = record(&d[at..(at + len).min(2048)], self.joliet).filter(|e| !e.name.is_empty()) {
                    out.push(e);
                }
                at += len;
            }
        }
        Ok(out)
    }

    /// The entry called `name` (any case) in `dir`.
    pub fn get(&mut self, dir: &Entry, name: &str) -> Option<Entry> {
        self.list(dir).ok()?.into_iter().find(|e| e.name.eq_ignore_ascii_case(name))
    }

    /// The first file called `name` in `dir` or below, with its folder.
    pub fn find(&mut self, dir: &Entry, name: &str, depth: u32) -> Option<(Entry, Entry)> {
        let list = self.list(dir).ok()?;
        if let Some(e) = list.iter().find(|e| !e.dir && e.name.eq_ignore_ascii_case(name)) {
            return Some((dir.clone(), e.clone()));
        }
        if depth == 0 {
            return None;
        }
        list.iter().filter(|e| e.dir).find_map(|d| self.find(d, name, depth - 1))
    }

    /// Copy a file's contents to `out`.
    pub fn read(&mut self, f: &Entry, out: &mut impl Write) -> std::io::Result<()> {
        self.copy(f, out)?;
        self.copied += f.size as u64;
        let copied = self.copied;
        if let Some(p) = &mut self.progress {
            p(copied);
        }
        Ok(())
    }

    /// The bytes of the files in a folder and below.
    pub fn size(&mut self, dir: &Entry) -> std::io::Result<u64> {
        let mut n = 0;
        for e in self.list(dir)? {
            n += if e.dir { self.size(&e)? } else { e.size as u64 };
        }
        Ok(n)
    }

    fn copy(&mut self, f: &Entry, out: &mut impl Write) -> std::io::Result<()> {
        if self.sector_size == 2048 {
            self.file.seek(SeekFrom::Start(f.sector as u64 * 2048))?;
            std::io::copy(&mut (&mut self.file).take(f.size as u64), out)?;
            return Ok(());
        }
        let mut left = f.size as usize;
        for k in 0.. {
            if left == 0 {
                break;
            }
            let d = self.sector(f.sector + k)?;
            let n = left.min(2048);
            out.write_all(&d[..n])?;
            left -= n;
        }
        Ok(())
    }

    /// Copy a folder with everything in it to `to`.
    pub fn extract(&mut self, dir: &Entry, to: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(to)?;
        for e in self.list(dir)? {
            let p = to.join(&e.name);
            if e.dir {
                self.extract(&e, &p)?;
            } else {
                self.read(&e, &mut std::io::BufWriter::new(File::create(&p)?))?;
            }
        }
        Ok(())
    }
}

impl Default for Entry {
    fn default() -> Self {
        Entry { name: String::new(), dir: true, sector: 0, size: 0 }
    }
}

/// A directory record (no name: a folder's own and its parent's).
fn record(r: &[u8], joliet: bool) -> Option<Entry> {
    if r.len() < 34 {
        return None;
    }
    let n = r[32] as usize;
    let raw = r.get(33..33 + n)?;
    let dir = r[25] & 2 != 0;
    let entry = |name| Entry {
        name,
        dir,
        sector: u32::from_le_bytes(r[2..6].try_into().unwrap()),
        size: u32::from_le_bytes(r[10..14].try_into().unwrap()),
    };
    if n == 1 && raw[0] <= 1 {
        // a folder's records for itself and its parent (and the root's in a volume descriptor)
        return Some(entry(String::new()));
    }
    let mut name = if joliet {
        let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        raw.iter().map(|&b| b as char).collect()
    };
    // version numbers (";1") and the dot of names without an extension
    if let Some(k) = name.find(';') {
        name.truncate(k);
    }
    if !dir && name.ends_with('.') {
        name.pop();
    }
    Some(entry(name))
}
