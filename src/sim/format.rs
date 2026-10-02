//! `$display` formatting (IEEE 1800-2023 §21.2.1).

use crate::eval::{Value, bits_info};
use crate::ir::Bits;
use crate::ir::{Format, FormatPiece, Type};

/// Format the pieces with their argument values.
pub fn format_display(
    f: &Format<'_>,
    args: &[(Value, &Type<'_>)],
    time: u64,
    unit: i8,
    precision: i8,
) -> String {
    let mut out = String::new();
    let mut next = args.iter();
    for p in &f.pieces {
        match p {
            FormatPiece::Text(t) => out.push_str(t),
            FormatPiece::Conv {
                spec,
                width,
                zero_pad,
                left,
            } => {
                let Some((v, ty)) = next.next() else { break };
                let s = conv(*spec, v, ty, *width, time, unit, precision);
                if *left {
                    // Left-justified: the value's own digits, then spaces.
                    let s = s.trim_start();
                    out.push_str(s);
                    let w = width.unwrap_or(0) as usize;
                    out.extend(std::iter::repeat_n(' ', w.saturating_sub(s.len())));
                } else {
                    pad(&mut out, &s, *width, *zero_pad, *spec);
                }
            }
        }
    }
    out
}

fn pad(out: &mut String, s: &str, width: Option<u32>, zero: bool, spec: char) {
    if let Some(w) = width {
        let w = w as usize;
        if s.len() < w {
            let fill = if zero && !matches!(spec, 's' | 'c') {
                '0'
            } else {
                ' '
            };
            out.extend(std::iter::repeat_n(fill, w - s.len()));
        }
    }
    out.push_str(s);
}

/// One conversion. `width` is `None` for the natural width, `Some(0)` for minimal.
fn conv(
    spec: char,
    v: &Value,
    ty: &Type<'_>,
    width: Option<u32>,
    _time: u64,
    unit: i8,
    precision: i8,
) -> String {
    let (w, signed) = bits_info(ty).map_or((64, false), |(w, s, _)| (w, s));
    let natural = width.is_none();
    match (spec, v) {
        ('e' | 'f' | 'g', v) => {
            let r = to_real(v, signed);
            match spec {
                'e' => format_e(r),
                'f' => format!("{r:.6}"),
                _ => format_g(r),
            }
        }
        ('t', v) => {
            // The default $timeformat: the simulation precision, no decimals, width 20.
            let r = to_real(v, signed) * 10f64.powi((unit - precision) as i32);
            let s = format!("{}", r.round() as i128);
            if natural { format!("{s:>20}") } else { s }
        }
        ('p', Value::Array(a)) => {
            // A fixed-size array's element 0 is its rightmost; a queue's its leftmost.
            let items: Vec<String> = if matches!(ty, Type::Unpacked { .. }) {
                a.iter().rev().map(pattern_elem).collect()
            } else {
                a.iter().map(pattern_elem).collect()
            };
            format!("'{{{}}}", items.join(", "))
        }
        ('p', Value::Bits(b))
            if matches!(ty, Type::Bits { fields: Some(f), .. } if !f.is_empty()) =>
        {
            // A packed struct: its members by name.
            let Type::Bits {
                fields: Some(f), ..
            } = ty
            else {
                unreachable!()
            };
            let mut items = Vec::new();
            // Members are most significant first; each runs up to the one before.
            let mut top = b.width;
            for fld in f {
                let w = top.saturating_sub(fld.offset).max(1);
                let v = b.select(fld.offset as i64, w, true);
                items.push(format!("{}:'h{}", fld.name, radix(&v, 4, false)));
                top = fld.offset;
            }
            format!("'{{{}}}", items.join(", "))
        }
        ('p', Value::Bits(b)) if natural => decimal(b, signed),
        ('p', v) => pattern_elem(v),
        ('s', Value::Str(s)) => s.clone(),
        ('s', Value::Real(r)) => format_g(*r),
        ('s', Value::Bits(b)) => {
            let mut bytes = Vec::new();
            for i in (0..b.width.div_ceil(8)).rev() {
                let byte = b.select(i as i64 * 8, 8, false).to_u64() as u8;
                bytes.push(byte);
            }
            let start = bytes.iter().position(|&c| c != 0).unwrap_or(bytes.len());
            let text: String = bytes[start..].iter().map(|&c| c as char).collect();
            if natural {
                // Unused leading bytes print as spaces.
                format!("{text:>width$}", width = b.width.div_ceil(8) as usize)
            } else if text.is_empty() {
                // Nothing to print still takes one place.
                " ".into()
            } else {
                text
            }
        }
        ('c', Value::Bits(b)) => ((b.to_u64() & 0xff) as u8 as char).to_string(),
        ('d' | 'u', Value::Bits(b)) => {
            let s = decimal(b, signed && spec == 'd');
            if natural {
                format!(
                    "{s:>width$}",
                    width = decimal_width(w, signed && spec == 'd')
                )
            } else {
                s
            }
        }
        ('h', Value::Bits(b)) => radix(b, 4, natural),
        ('o', Value::Bits(b)) => radix(b, 3, natural),
        ('b', Value::Bits(b)) => radix(b, 1, natural),
        ('d' | 'h' | 'o' | 'b', Value::Real(r)) => {
            let b = Bits::from_i64(64, r.round() as i64);
            conv(
                spec,
                &Value::Bits(b),
                &Type::Bits {
                    width: 64,
                    signed: true,
                    four_state: false,
                    fields: None,
                    names: None,
                },
                width,
                _time,
                unit,
                precision,
            )
        }
        (_, Value::Str(s)) => s.clone(),
        _ => "?".into(),
    }
}

/// A value as it appears inside `%p` output.
fn pattern_elem(v: &Value) -> String {
    match v {
        Value::Bits(b) => format!("'h{}", radix(b, 4, false)),
        Value::Real(r) => format_g(*r),
        Value::Str(s) => format!("{s:?}"),
        Value::Array(a) => {
            let items: Vec<String> = a.iter().rev().map(pattern_elem).collect();
            format!("'{{{}}}", items.join(", "))
        }
        Value::Obj(None) => "null".into(),
        Value::Obj(Some(o)) => {
            let o = o.0.borrow();
            let items: Vec<String> = o
                .fields
                .iter()
                .zip(o.names.iter())
                .map(|(v, n)| format!("{n}:{}", pattern_elem(v)))
                .collect();
            format!("'{{{}}}", items.join(", "))
        }
    }
}

fn to_real(v: &Value, signed: bool) -> f64 {
    match v {
        Value::Real(r) => *r,
        Value::Bits(b) => crate::eval::bits_to_f64(b, signed),
        Value::Str(_) | Value::Array(_) | Value::Obj(_) => 0.0,
    }
}

/// Characters needed for the largest value of a `w`-bit type.
fn decimal_width(w: u32, signed: bool) -> usize {
    let max = if signed {
        Bits::ones(w.max(1)).shr(1, false)
    } else {
        Bits::ones(w.max(1))
    };
    decimal(&max, false).len() + usize::from(signed)
}

/// Decimal digits; `x`/`X`/`z`/`Z` for unknown values (LRM 21.2.1.3).
fn decimal(b: &Bits, signed: bool) -> String {
    if let Some(u) = &b.unknown {
        let all = (0..b.width).all(|i| b.bit(i).1);
        let all_z = (0..b.width).all(|i| b.bit(i) == (true, true));
        let any_z = (0..b.width).any(|i| b.bit(i) == (true, true));
        let _ = u;
        return match (all, all_z, any_z) {
            (true, true, _) => "z",
            (true, false, _) => "x",
            (false, _, true) => "Z",
            _ => "X",
        }
        .into();
    }
    let neg = signed && b.msb().0;
    let mag = if neg { b.neg() } else { b.clone() };
    let mut digits = String::new();
    if mag.width <= 128 {
        let v = mag.val[0] as u128 | (mag.val.get(1).copied().unwrap_or(0) as u128) << 64;
        digits = v.to_string();
    } else {
        // Short division by 10^18 over 64-bit limbs.
        let mut limbs: Vec<u64> = mag.val.clone();
        let mut chunks = Vec::new();
        while limbs.iter().any(|&l| l != 0) {
            let mut rem: u128 = 0;
            for l in limbs.iter_mut().rev() {
                let cur = (rem << 64) | *l as u128;
                *l = (cur / 1_000_000_000_000_000_000) as u64;
                rem = cur % 1_000_000_000_000_000_000;
            }
            chunks.push(rem as u64);
        }
        for (i, c) in chunks.iter().rev().enumerate() {
            if i == 0 {
                digits.push_str(&c.to_string());
            } else {
                digits.push_str(&format!("{c:018}"));
            }
        }
        if digits.is_empty() {
            digits.push('0');
        }
    }
    if neg { format!("-{digits}") } else { digits }
}

/// Hex, octal or binary digits, `bits` per digit. X/Z digits are `x`/`z`
/// when every bit of the digit is, `X`/`Z` when only some are.
fn radix(b: &Bits, bits: u32, natural: bool) -> String {
    let n = b.width.div_ceil(bits);
    let mut s = String::new();
    for d in (0..n).rev() {
        let lo = d * bits;
        let mut v = 0u32;
        let (mut nx, mut nz, mut cnt) = (0, 0, 0);
        for k in 0..bits {
            let i = lo + k;
            if i >= b.width {
                continue;
            }
            cnt += 1;
            match b.bit(i) {
                (true, false) => v |= 1 << k,
                (false, true) => nx += 1,
                (true, true) => nz += 1,
                _ => {}
            }
        }
        s.push(if nx == cnt {
            'x'
        } else if nz == cnt {
            'z'
        } else if nx > 0 {
            'X'
        } else if nz > 0 {
            'Z'
        } else {
            std::char::from_digit(v, 16).unwrap()
        });
    }
    if !natural {
        let t = s.trim_start_matches('0');
        return if t.is_empty() { "0".into() } else { t.into() };
    }
    s
}

fn format_e(r: f64) -> String {
    let s = format!("{r:.6e}");
    // Rust writes 1.5e3; C writes 1.500000e+03.
    match s.split_once('e') {
        Some((m, e)) => {
            let (sign, digits) = match e.strip_prefix('-') {
                Some(d) => ('-', d),
                None => ('+', e),
            };
            format!("{m}e{sign}{digits:0>2}")
        }
        None => s,
    }
}

/// `%g`, as C prints it.
pub fn format_g(r: f64) -> String {
    if r == 0.0 {
        return "0".into();
    }
    let exp = r.abs().log10().floor() as i32;
    if !(-4..6).contains(&exp) {
        let s = format_e(r);
        let (m, e) = s.split_once('e').unwrap();
        let m = m.trim_end_matches('0').trim_end_matches('.');
        return format!("{m}e{e}");
    }
    let decimals = (5 - exp).max(0) as usize;
    let s = format!("{r:.decimals$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(w: u32, s: bool) -> Type<'static> {
        Type::Bits {
            width: w,
            signed: s,
            four_state: true,
            fields: None,
            names: None,
        }
    }

    fn one(spec: char, width: Option<u32>, v: Bits, t: &Type) -> String {
        let f = Format {
            pieces: vec![FormatPiece::Conv {
                spec,
                width,
                zero_pad: false,
                left: false,
            }],
        };
        format_display(&f, &[(Value::Bits(v), t)], 0, -9, -12)
    }

    #[test]
    fn natural_and_minimal_widths() {
        let t8 = ty(8, false);
        assert_eq!(one('d', None, Bits::from_u64(8, 5), &t8), "  5");
        assert_eq!(one('d', Some(0), Bits::from_u64(8, 5), &t8), "5");
        assert_eq!(one('h', None, Bits::from_u64(8, 5), &t8), "05");
        assert_eq!(one('h', Some(0), Bits::from_u64(8, 5), &t8), "5");
        assert_eq!(one('b', None, Bits::from_u64(4, 5), &ty(4, false)), "0101");
        assert_eq!(one('d', None, Bits::from_i64(8, -3), &ty(8, true)), "  -3");
        assert_eq!(
            one('d', None, Bits::from_u64(32, 7), &ty(32, false)),
            "         7"
        );
    }

    #[test]
    fn unknown_digits() {
        let t8 = ty(8, false);
        assert_eq!(one('d', Some(0), Bits::all_x(8), &t8), "x");
        assert_eq!(one('h', None, Bits::all_x(8), &t8), "xx");
        let mut b = Bits::from_u64(8, 0x1f);
        b.insert(4, &Bits::all_z(2));
        assert_eq!(one('h', None, b, &t8), "Zf");
    }

    #[test]
    fn strings_time_and_reals() {
        let f = Format {
            pieces: vec![FormatPiece::Conv {
                spec: 's',
                width: Some(0),
                zero_pad: false,
                left: false,
            }],
        };
        let abc = Bits::concat(&[
            Bits::from_u64(8, b'a' as u64),
            Bits::from_u64(8, b'b' as u64),
        ]);
        assert_eq!(
            format_display(&f, &[(Value::Bits(abc), &ty(16, false))], 0, -9, -12),
            "ab"
        );
        // 5 ns at 1 ps precision.
        assert_eq!(
            one('t', Some(0), Bits::from_u64(64, 5), &ty(64, false)),
            "5000"
        );
        assert_eq!(format_e(1500.0), "1.500000e+03");
        assert_eq!(format_g(0.5), "0.5");
    }
}
