//! Dialog templates (RT_DIALOG resources): DLGTEMPLATE and DLGTEMPLATEEX, parsed into plain
//! Rust values. Coordinates stay in dialog units.

/// Window class of a control.
#[derive(Clone, Debug, PartialEq)]
pub enum Class {
    Button,
    Edit,
    Static,
    ListBox,
    ScrollBar,
    ComboBox,
    /// any other class by name (e.g. msctls_trackbar32)
    Other(String),
}

#[derive(Clone, Debug)]
pub struct Item {
    pub class: Class,
    pub id: u32,
    pub text: String,
    pub style: u32,
    pub ex_style: u32,
    /// x, y, width, height in dialog units
    pub rect: [i32; 4],
}

#[derive(Clone, Debug)]
pub struct Template {
    pub style: u32,
    pub title: String,
    pub rect: [i32; 4],
    pub font: Option<(u16, String)>,
    pub items: Vec<Item>,
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn u16(&mut self) -> Option<u16> {
        let v = u16::from_le_bytes([*self.b.get(self.at)?, *self.b.get(self.at + 1)?]);
        self.at += 2;
        Some(v)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(self.u16()? as u32 | (self.u16()? as u32) << 16)
    }
    fn i16(&mut self) -> Option<i32> {
        Some(self.u16()? as i16 as i32)
    }
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.at)?;
        self.at += 1;
        Some(v)
    }
    fn align4(&mut self) {
        self.at = (self.at + 3) & !3;
    }
    /// A NUL-terminated UTF-16 string.
    fn wstr(&mut self) -> Option<String> {
        let mut s = String::new();
        loop {
            match self.u16()? {
                0 => return Some(s),
                c => s.push(char::from_u32(c as u32).unwrap_or('?')),
            }
        }
    }
    /// A string or an ordinal (0xffff + number); none is an empty string.
    fn sz_or_ord(&mut self) -> Option<Result<String, u16>> {
        match self.u16()? {
            0 => Some(Ok(String::new())),
            0xffff => Some(Err(self.u16()?)),
            _ => {
                self.at -= 2;
                Some(Ok(self.wstr()?))
            }
        }
    }
}

const DS_SETFONT: u32 = 0x40;

impl Template {
    pub fn parse(b: &[u8]) -> Option<Template> {
        let mut r = Reader { b, at: 0 };
        let ex = b.len() >= 4 && b[0..2] == [1, 0] && b[2..4] == [0xff, 0xff];
        let (style, count);
        if ex {
            r.at = 4;
            let _help = r.u32()?;
            let _ex_style = r.u32()?;
            style = r.u32()?;
        } else {
            style = r.u32()?;
            let _ex_style = r.u32()?;
        }
        count = r.u16()?;
        let rect = [r.i16()?, r.i16()?, r.i16()?, r.i16()?];
        let _menu = r.sz_or_ord()?;
        let _class = r.sz_or_ord()?;
        let title = r.wstr()?;
        let font = if style & DS_SETFONT != 0 {
            let size = r.u16()?;
            if ex {
                let _weight = r.u16()?;
                let _italic = r.u8()?;
                let _charset = r.u8()?;
            }
            Some((size, r.wstr()?))
        } else {
            None
        };
        let mut items = Vec::new();
        for _ in 0..count {
            r.align4();
            let (style, ex_style, id);
            let rect;
            if ex {
                let _help = r.u32()?;
                ex_style = r.u32()?;
                style = r.u32()?;
                rect = [r.i16()?, r.i16()?, r.i16()?, r.i16()?];
                id = r.u32()?;
            } else {
                style = r.u32()?;
                ex_style = r.u32()?;
                rect = [r.i16()?, r.i16()?, r.i16()?, r.i16()?];
                id = r.u16()? as u32;
            }
            let class = match r.sz_or_ord()? {
                Err(0x80) => Class::Button,
                Err(0x81) => Class::Edit,
                Err(0x82) => Class::Static,
                Err(0x83) => Class::ListBox,
                Err(0x84) => Class::ScrollBar,
                Err(0x85) => Class::ComboBox,
                Err(n) => Class::Other(format!("#{n}")),
                Ok(name) => match name.to_ascii_lowercase().as_str() {
                    "button" => Class::Button,
                    "edit" => Class::Edit,
                    "static" => Class::Static,
                    "listbox" => Class::ListBox,
                    "scrollbar" => Class::ScrollBar,
                    "combobox" => Class::ComboBox,
                    _ => Class::Other(name),
                },
            };
            let text = match r.sz_or_ord()? {
                Ok(s) => s,
                Err(n) => format!("#{n}"),
            };
            let extra = r.u16()? as usize;
            r.at += extra;
            // 16-bit ids: 0xffff is "no id"
            let id = if !ex && id == 0xffff { u32::MAX } else { id };
            items.push(Item { class, id, text, style, ex_style, rect });
        }
        Some(Template { style, title, rect, font, items })
    }
}
