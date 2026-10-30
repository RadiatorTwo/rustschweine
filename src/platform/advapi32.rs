//! ADVAPI32: the registry, kept in `registry.txt` in the game folder:
//!
//! ```text
//! [HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Hogs Of War]
//! Footprint = dword:00000001
//! LanguageLetter = sz:G
//! InstallSource = hex(1):443a5c00
//! ```
//!
//! Strings with a backslash are written in hex (`hex(1)`, REG_SZ, with its NUL).

use super::{Import, arg, cstr, game_dir, log, mem, put_cstr};
use crate::rt::Cpu;
use std::collections::BTreeMap;
use std::sync::Mutex;

const OK: u32 = 0;
const NOT_FOUND: u32 = 2;
const MORE_DATA: u32 = 234;
const REG_SZ: u32 = 1;
const REG_DWORD: u32 = 4;

/// What the installer writes and the game reads at its start (0x481d00).
pub const HOGS_KEY: &str = "HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Hogs Of War";

struct Registry {
    loaded: bool,
    /// key path (as written first) -> value name -> (type, data); looked up case-insensitively
    keys: BTreeMap<String, BTreeMap<String, (u32, Vec<u8>)>>,
    open: BTreeMap<u32, String>,
    next: u32,
}

static REG: Mutex<Registry> = Mutex::new(Registry { loaded: false, keys: BTreeMap::new(), open: BTreeMap::new(), next: 0x1000 });

fn reg() -> std::sync::MutexGuard<'static, Registry> {
    let mut r = REG.lock().unwrap_or_else(|e| e.into_inner());
    if !r.loaded {
        r.loaded = true;
        r.load();
    }
    r
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|k| u8::from_str_radix(&s[2 * k..2 * k + 2], 16).ok()).collect()
}

impl Registry {
    fn load(&mut self) {
        // as the installer leaves them: installed in C:\Hogs from the CD in D: (both the game
        // folder, see `host_path`), German (LanguageLetter G: "Frontschweine", Language        // folders)
        let k = self.key_mut(HOGS_KEY);
        k.insert("InstallSource".into(), (REG_SZ, b"D:\\\0".to_vec()));
        k.insert("InstallLocation".into(), (REG_SZ, b"C:\\Hogs\\\0".to_vec()));
        k.insert("LanguageLetter".into(), (REG_SZ, b"G\0".to_vec()));
        if let Ok(text) = std::fs::read_to_string(game_dir().join("registry.txt")) {
            let mut cur = None;
            for line in text.lines().map(str::trim) {
                if let Some(p) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                    self.key_mut(p);
                    cur = Some(p.to_string());
                } else if let (Some(k), Some((name, v))) = (&cur, line.split_once('=')) {
                    let (name, v) = (name.trim(), v.trim());
                    let val = if let Some(d) = v.strip_prefix("dword:") {
                        (REG_DWORD, u32::from_str_radix(d, 16).unwrap_or(0).to_le_bytes().to_vec())
                    } else if let Some(s) = v.strip_prefix("sz:") {
                        let mut b: Vec<u8> = s.chars().map(|c| c as u8).collect();
                        b.push(0);
                        (REG_SZ, b)
                    } else if let Some((t, d)) = v.split_once(":").and_then(|(t, d)| Some((t.strip_prefix("hex")?, d))) {
                        (t.trim_matches(['(', ')']).parse().unwrap_or(3), unhex(d))
                    } else {
                        continue;
                    };
                    let k = k.clone();
                    self.key_mut(&k).insert(name.to_string(), val);
                }
            }
        }
        // the installation's size; the game reads it only for the videos (0x43ee10): 2 skips
        // them, 0 and 1 play them from InstallSource. Set after registry.txt, which kept the 2
        // we wrote before the videos played
        let k = self.key_mut(HOGS_KEY);
        k.insert("Footprint".into(), (REG_DWORD, 1u32.to_le_bytes().to_vec()));
    }

    fn save(&self) {
        let mut out = String::new();
        for (k, vals) in &self.keys {
            out += &format!("[{k}]\n");
            for (name, (t, d)) in vals {
                let v = match *t {
                    REG_DWORD if d.len() == 4 => format!("dword:{:08x}", u32::from_le_bytes(d[..4].try_into().unwrap())),
                    REG_SZ if d.last() == Some(&0) && d[..d.len() - 1].iter().all(|&b| b >= 32 && b != b'\\' || b >= 128) => {
                        format!("sz:{}", d[..d.len() - 1].iter().map(|&b| b as char).collect::<String>())
                    }
                    t => format!("hex({t}):{}", hex(d)),
                };
                out += &format!("{name} = {v}\n");
            }
            out += "\n";
        }
        if let Err(e) = std::fs::write(game_dir().join("registry.txt"), out) {
            log!("warn: cannot save registry.txt: {e}");
        }
    }

    fn find(&self, path: &str) -> Option<&String> {
        self.keys.keys().find(|k| k.eq_ignore_ascii_case(path))
    }

    fn key_mut(&mut self, path: &str) -> &mut BTreeMap<String, (u32, Vec<u8>)> {
        let k = self.find(path).cloned().unwrap_or_else(|| path.to_string());
        self.keys.entry(k).or_default()
    }

    fn exists(&self, path: &str) -> bool {
        let p = path.to_ascii_lowercase();
        !p.contains('\\')
            || p == "hkey_local_machine\\software"
            || p == "hkey_current_user\\software"
            || self.keys.keys().any(|k| {
                let k = k.to_ascii_lowercase();
                k == p || k.starts_with(&p) && k.as_bytes()[p.len()] == b'\\'
            })
    }

    /// The full path of `sub` below the open key or root key `h`.
    fn path(&self, h: u32, sub: u32) -> Option<String> {
        let base = match h {
            0x8000_0000 => "HKEY_CLASSES_ROOT".to_string(),
            0x8000_0001 => "HKEY_CURRENT_USER".to_string(),
            0x8000_0002 => "HKEY_LOCAL_MACHINE".to_string(),
            0x8000_0003 => "HKEY_USERS".to_string(),
            _ => self.open.get(&h)?.clone(),
        };
        let sub = if sub == 0 { String::new() } else { cstr(sub) };
        let sub = sub.trim_matches('\\');
        Some(if sub.is_empty() { base } else { format!("{base}\\{sub}") })
    }

    fn open_key(&mut self, path: String) -> u32 {
        self.next += 4;
        self.open.insert(self.next, path);
        self.next
    }
}

/// Write the registry file (the game saves its settings when it exits).
pub fn save() {
    reg().save();
}

/// A value of the game's key or one below it, e.g. ("Settings", "Resolution").
pub fn value(sub: &str, name: &str) -> Option<(u32, Vec<u8>)> {
    let r = reg();
    let path = if sub.is_empty() { HOGS_KEY.to_string() } else { format!("{HOGS_KEY}\\{sub}") };
    let k = r.find(&path)?;
    r.keys[k].iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
}

/// Set a value of the game's key or one below it and save the registry file.
pub fn set_value(sub: &str, name: &str, t: u32, data: &[u8]) {
    let mut r = reg();
    let path = if sub.is_empty() { HOGS_KEY.to_string() } else { format!("{HOGS_KEY}\\{sub}") };
    let key = r.key_mut(&path);
    let existing = key.keys().find(|k| k.eq_ignore_ascii_case(name)).cloned().unwrap_or_else(|| name.to_string());
    key.insert(existing, (t, data.to_vec()));
    r.save();
}

fn trace() -> bool {
    static T: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *T.get_or_init(|| std::env::var_os("HOGS_TRACE_REG").is_some())
}

fn reg_open_key_ex(c: &mut Cpu) -> u32 {
    let (h, sub, out) = (arg(c, 0), arg(c, 1), arg(c, 4));
    let mut r = reg();
    let Some(p) = r.path(h, sub) else { return NOT_FOUND };
    if trace() {
        log!("reg: open {p}");
    }
    if !r.exists(&p) {
        return NOT_FOUND;
    }
    let k = r.open_key(p);
    mem().wr32(out, k);
    OK
}

fn reg_create_key_ex(c: &mut Cpu) -> u32 {
    let (h, sub, out, disp) = (arg(c, 0), arg(c, 1), arg(c, 7), arg(c, 8));
    let mut r = reg();
    let Some(p) = r.path(h, sub) else { return NOT_FOUND };
    let existed = r.exists(&p);
    if trace() {
        log!("reg: create {p}");
    }
    r.key_mut(&p);
    let k = r.open_key(p);
    mem().wr32(out, k);
    if disp != 0 {
        mem().wr32(disp, if existed { 2 } else { 1 });
    }
    OK
}

fn reg_close_key(c: &mut Cpu) -> u32 {
    reg().open.remove(&arg(c, 0));
    OK
}

fn reg_query_value_ex(c: &mut Cpu) -> u32 {
    let (h, name, ty, data, len) = (arg(c, 0), arg(c, 1), arg(c, 3), arg(c, 4), arg(c, 5));
    let r = reg();
    let name = if name == 0 { String::new() } else { cstr(name) };
    let Some(p) = r.path(h, 0) else { return NOT_FOUND };
    let v = r.find(&p).and_then(|k| r.keys[k].iter().find(|(n, _)| n.eq_ignore_ascii_case(&name))).map(|(_, v)| v.clone());
    if trace() {
        log!("reg: query {p} {name} -> {v:?}");
    }
    let Some((t, d)) = v else { return NOT_FOUND };
    let m = mem();
    if ty != 0 {
        m.wr32(ty, t);
    }
    if len == 0 {
        return OK;
    }
    let room = m.rd32(len);
    m.wr32(len, d.len() as u32);
    if data == 0 {
        return OK;
    }
    if (room as usize) < d.len() {
        return MORE_DATA;
    }
    m.slice(data, d.len()).copy_from_slice(&d);
    OK
}

fn reg_set_value_ex(c: &mut Cpu) -> u32 {
    let (h, name, t, data, n) = (arg(c, 0), arg(c, 1), arg(c, 3), arg(c, 4), arg(c, 5));
    let mut r = reg();
    let name = if name == 0 { String::new() } else { cstr(name) };
    let Some(p) = r.path(h, 0) else { return NOT_FOUND };
    let d = mem().slice(data, n as usize).to_vec();
    if trace() {
        log!("reg: set {p} {name} = {t} {d:?}");
    }
    let key = r.key_mut(&p);
    let existing = key.keys().find(|k| k.eq_ignore_ascii_case(&name)).cloned().unwrap_or(name);
    key.insert(existing, (t, d));
    OK
}

fn reg_flush_key(_: &mut Cpu) -> u32 {
    reg().save();
    OK
}

fn get_user_name(c: &mut Cpu) -> u32 {
    let (buf, len) = (arg(c, 0), arg(c, 1));
    let k = put_cstr(buf, "Player", mem().rd32(len));
    mem().wr32(len, k + 1);
    1
}

pub const IMPORTS: &[Import] = &[
    ("RegOpenKeyExA", 5, reg_open_key_ex),
    ("RegCreateKeyExA", 9, reg_create_key_ex),
    ("RegCloseKey", 1, reg_close_key),
    ("RegQueryValueExA", 6, reg_query_value_ex),
    ("RegSetValueExA", 6, reg_set_value_ex),
    ("RegFlushKey", 1, reg_flush_key),
    ("GetUserNameA", 2, get_user_name),
];
