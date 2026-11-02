//! 暫存器值的顯示格式轉換（Decimal / Integer / Hex / Binary / Float …）與寫入時的反向解析。

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Func {
    Coils,
    DiscreteInputs,
    HoldingRegisters,
    InputRegisters,
}

impl Func {
    pub const ALL: [Func; 4] = [
        Func::Coils,
        Func::DiscreteInputs,
        Func::HoldingRegisters,
        Func::InputRegisters,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Func::Coils => "Coil status (FC01)",
            Func::DiscreteInputs => "Input status (FC02)",
            Func::HoldingRegisters => "Holding registers (FC03)",
            Func::InputRegisters => "Input registers (FC04)",
        }
    }
    pub fn is_bit(self) -> bool {
        matches!(self, Func::Coils | Func::DiscreteInputs)
    }
    pub fn writable(self) -> bool {
        matches!(self, Func::Coils | Func::HoldingRegisters)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Display {
    Decimal,
    Integer,
    Hex,
    Binary,
    U32,
    U32Sw,
    I32,
    I32Sw,
    F32,
    F32Sw,
    F64,
    F64Sw,
}

impl Display {
    pub fn label(self) -> &'static str {
        match self {
            Display::Decimal => "Decimal",
            Display::Integer => "Integer",
            Display::Hex => "Hexadecimal",
            Display::Binary => "Binary",
            Display::U32 => "32bit unsigned",
            Display::U32Sw => "32bit sw. unsigned",
            Display::I32 => "32bit integer",
            Display::I32Sw => "32bit sw. integer",
            Display::F32 => "32bit float",
            Display::F32Sw => "32bit sw. float",
            Display::F64 => "64bit float",
            Display::F64Sw => "64bit sw. float",
        }
    }
    /// 一個值佔用幾個暫存器
    pub fn width(self) -> usize {
        match self {
            Display::Decimal | Display::Integer | Display::Hex | Display::Binary => 1,
            Display::F64 | Display::F64Sw => 4,
            _ => 2,
        }
    }
    /// sw. = word swap（低位 word 在前，即 CDAB）
    fn swapped(self) -> bool {
        matches!(self, Display::U32Sw | Display::I32Sw | Display::F32Sw | Display::F64Sw)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ascii {
    Off,
    OneChar,
    TwoChar,
    TwoCharSw,
}

/// 把 N 個暫存器依 word 順序組成大端序 bytes
fn words_to_be(words: &[u16], swapped: bool) -> Vec<u8> {
    let mut w: Vec<u16> = words.to_vec();
    if swapped {
        w.reverse();
    }
    w.iter().flat_map(|x| x.to_be_bytes()).collect()
}

fn be_to_words(bytes: &[u8], swapped: bool) -> Vec<u16> {
    let mut w: Vec<u16> = bytes
        .chunks(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    if swapped {
        w.reverse();
    }
    w
}

fn printable(b: u8) -> char {
    if (0x20..0x7f).contains(&b) {
        b as char
    } else {
        '.'
    }
}

fn ascii_of(v: u16, mode: Ascii) -> String {
    let [hi, lo] = v.to_be_bytes();
    match mode {
        Ascii::Off => String::new(),
        Ascii::OneChar => format!(" '{}'", printable(lo)),
        Ascii::TwoChar => format!(" '{}{}'", printable(hi), printable(lo)),
        Ascii::TwoCharSw => format!(" '{}{}'", printable(lo), printable(hi)),
    }
}

/// 回傳與 values 等長的字串；多暫存器格式只在第一格顯示，其餘為空字串。
pub fn format_values(values: &[u16], func: Func, disp: Display, ascii: Ascii) -> Vec<String> {
    if func.is_bit() {
        return values.iter().map(|v| v.to_string()).collect();
    }
    let width = disp.width();
    let mut out = vec![String::new(); values.len()];
    let mut i = 0;
    while i < values.len() {
        let s = if width == 1 {
            let v = values[i];
            let base = match disp {
                Display::Decimal => v.to_string(),
                Display::Integer => (v as i16).to_string(),
                Display::Hex => format!("0x{v:04X}"),
                _ => format!("{:08b} {:08b}", v >> 8, v & 0xff),
            };
            base + &ascii_of(v, ascii)
        } else if i + width <= values.len() {
            let b = words_to_be(&values[i..i + width], disp.swapped());
            match disp {
                Display::U32 | Display::U32Sw => u32::from_be_bytes(b[..4].try_into().unwrap()).to_string(),
                Display::I32 | Display::I32Sw => i32::from_be_bytes(b[..4].try_into().unwrap()).to_string(),
                Display::F32 | Display::F32Sw => fmt_float(f32::from_be_bytes(b[..4].try_into().unwrap()) as f64),
                _ => fmt_float(f64::from_be_bytes(b[..8].try_into().unwrap())),
            }
        } else {
            "-".into() // 讀取長度不足以組成完整數值
        };
        out[i] = s;
        i += width;
    }
    out
}

fn fmt_float(f: f64) -> String {
    if f.is_finite() && f.abs() < 1e9 && (f == 0.0 || f.abs() >= 1e-4) {
        format!("{f:.4}")
    } else {
        format!("{f:e}")
    }
}

/// 依目前顯示格式，把使用者輸入的字串轉成要寫入的暫存器（1、2 或 4 個 word）
pub fn parse_for_write(text: &str, disp: Display) -> Result<Vec<u16>, String> {
    let t = text.trim();
    let err = |_: std::num::ParseIntError| format!("無法解析「{t}」");
    let errf = |_: std::num::ParseFloatError| format!("無法解析「{t}」");
    let parse_u16 = |t: &str| -> Result<u16, String> {
        if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
            u16::from_str_radix(h, 16).map_err(|_| format!("無法解析「{t}」"))
        } else {
            t.parse::<u16>().map_err(|_| format!("無法解析「{t}」(0–65535)"))
        }
    };
    let words = match disp {
        Display::Decimal | Display::Hex => vec![parse_u16(t)?],
        Display::Binary => vec![u16::from_str_radix(&t.replace([' ', '_'], ""), 2).map_err(err)?],
        Display::Integer => vec![t.parse::<i16>().map_err(err)? as u16],
        Display::U32 | Display::U32Sw => be_to_words(&t.parse::<u32>().map_err(err)?.to_be_bytes(), disp.swapped()),
        Display::I32 | Display::I32Sw => be_to_words(&t.parse::<i32>().map_err(err)?.to_be_bytes(), disp.swapped()),
        Display::F32 | Display::F32Sw => be_to_words(&t.parse::<f32>().map_err(errf)?.to_be_bytes(), disp.swapped()),
        Display::F64 | Display::F64Sw => be_to_words(&t.parse::<f64>().map_err(errf)?.to_be_bytes(), disp.swapped()),
    };
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_roundtrip_both_orders() {
        for d in [Display::F32, Display::F32Sw, Display::F64, Display::F64Sw, Display::I32Sw] {
            let w = parse_for_write("-20.5", d).unwrap_or_else(|_| parse_for_write("-20", d).unwrap());
            let s = format_values(&w, Func::HoldingRegisters, d, Ascii::Off);
            assert!(s[0].starts_with("-20"), "{d:?} -> {s:?}");
        }
    }

    #[test]
    fn f32_abcd_vs_cdab() {
        // 20.07 = 0x41A08F5C
        let abcd = format_values(&[0x41A0, 0x8F5C], Func::HoldingRegisters, Display::F32, Ascii::Off);
        let cdab = format_values(&[0x8F5C, 0x41A0], Func::HoldingRegisters, Display::F32Sw, Ascii::Off);
        assert_eq!(abcd[0], "20.0700");
        assert_eq!(cdab[0], "20.0700");
        assert_eq!(abcd[1], "");
    }

    #[test]
    fn ascii_modes() {
        let s = format_values(&[0x5555], Func::HoldingRegisters, Display::Decimal, Ascii::TwoChar);
        assert_eq!(s[0], "21845 'UU'");
        let s = format_values(&[123], Func::HoldingRegisters, Display::Decimal, Ascii::OneChar);
        assert_eq!(s[0], "123 '{'");
    }
}
