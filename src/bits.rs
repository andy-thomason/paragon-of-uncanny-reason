//! Bit-vector arithmetic on [`Bits`]: the value semantics of IEEE 1800
//! clause 11, shared by constant evaluation and simulation.
//!
//! Encoding: `val` holds the bits, least significant 64-bit word first. A
//! 4-state value also has an `unknown` plane: where it is 0 the bit is `val`;
//! where it is 1, `val` 0 means X and `val` 1 means Z. Bits above `width` are
//! always 0 in both planes.
//!
//! Operations that the LRM makes unknown when any input bit is X or Z
//! (arithmetic, relational) return an all-X result. Bitwise operations follow
//! the per-bit truth tables. Callers convert to 2-state with
//! [`Bits::to_two_state`] when the result type is 2-state.

use crate::ir::Bits;
use std::cmp::Ordering;

fn words(width: u32) -> usize {
    (width as usize).div_ceil(64).max(1)
}

impl Bits {
    pub fn zero(width: u32) -> Bits {
        Bits {
            width,
            val: vec![0; words(width)],
            unknown: None,
        }
    }

    pub fn from_u64(width: u32, v: u64) -> Bits {
        let mut b = Bits::zero(width);
        b.val[0] = v;
        b.mask();
        b
    }

    /// `v` sign-extended to `width`.
    pub fn from_i64(width: u32, v: i64) -> Bits {
        let mut b = Bits::zero(width);
        for (i, w) in b.val.iter_mut().enumerate() {
            *w = if i == 0 {
                v as u64
            } else if v < 0 {
                u64::MAX
            } else {
                0
            };
        }
        b.mask();
        b
    }

    /// An integral value of a real, which should already be rounded or
    /// truncated; bits above `width` are dropped. NaN and infinities are 0.
    pub fn from_f64(width: u32, r: f64) -> Bits {
        if !r.is_finite() {
            return Bits::zero(width);
        }
        if r.abs() < 9.2e18 {
            return Bits::from_i64(width, r as i64);
        }
        // mantissa * 2^exp, built at the full width.
        let bits = r.abs().to_bits();
        let exp = ((bits >> 52) & 0x7ff) as i64 - 1075;
        let mantissa = (bits & ((1 << 52) - 1)) | (1 << 52);
        let m = Bits::from_u64(width.max(64), mantissa).shl(exp.max(0) as u64);
        let m = if r < 0.0 { m.neg() } else { m };
        m.resize(width, false)
    }

    pub fn from_bool(v: bool) -> Bits {
        Bits::from_u64(1, v as u64)
    }

    pub fn ones(width: u32) -> Bits {
        let mut b = Bits {
            width,
            val: vec![u64::MAX; words(width)],
            unknown: None,
        };
        b.mask();
        b
    }

    pub fn all_x(width: u32) -> Bits {
        let mut b = Bits {
            width,
            val: vec![0; words(width)],
            unknown: Some(vec![u64::MAX; words(width)]),
        };
        b.mask();
        b
    }

    pub fn all_z(width: u32) -> Bits {
        let mut b = Bits {
            width,
            val: vec![u64::MAX; words(width)],
            unknown: Some(vec![u64::MAX; words(width)]),
        };
        b.mask();
        b
    }

    /// Clear bits above `width`, and drop an all-zero unknown plane.
    fn mask(&mut self) {
        let n = words(self.width);
        self.val.resize(n, 0);
        let top = self.width % 64;
        let m = if top == 0 {
            u64::MAX
        } else {
            (1u64 << top) - 1
        };
        self.val[n - 1] &= m;
        if let Some(u) = &mut self.unknown {
            u.resize(n, 0);
            u[n - 1] &= m;
            if u.iter().all(|&w| w == 0) {
                self.unknown = None;
            }
        }
    }

    pub fn has_unknown(&self) -> bool {
        self.unknown.is_some()
    }

    fn unk(&self, i: usize) -> u64 {
        self.unknown.as_ref().map_or(0, |u| u[i])
    }

    /// Bit `i` as (value, unknown).
    pub fn bit(&self, i: u32) -> (bool, bool) {
        let (w, b) = ((i / 64) as usize, i % 64);
        ((self.val[w] >> b) & 1 == 1, (self.unk(w) >> b) & 1 == 1)
    }

    pub fn set_bit(&mut self, i: u32, v: bool, u: bool) {
        let (w, b) = ((i / 64) as usize, i % 64);
        self.val[w] = (self.val[w] & !(1 << b)) | ((v as u64) << b);
        if u || self.unknown.is_some() {
            let n = self.val.len();
            let p = self.unknown.get_or_insert_with(|| vec![0; n]);
            p[w] = (p[w] & !(1 << b)) | ((u as u64) << b);
        }
    }

    /// The most significant bit, as (value, unknown).
    pub fn msb(&self) -> (bool, bool) {
        self.bit(self.width - 1)
    }

    /// Turn X and Z bits into 0, as storing into a 2-state variable does.
    pub fn to_two_state(&mut self) {
        if let Some(u) = self.unknown.take() {
            for (v, u) in self.val.iter_mut().zip(u) {
                *v &= !u;
            }
        }
    }

    /// The low 64 bits, unsigned. Unknown bits read as 0.
    pub fn to_u64(&self) -> u64 {
        self.val[0] & !self.unk(0)
    }

    /// The value as an integer, if it is known and fits in an `i64`.
    pub fn to_i64(&self, signed: bool) -> Option<i64> {
        if self.has_unknown() {
            return None;
        }
        if self.width < 64 {
            let v = self.val[0];
            let neg = signed && self.msb().0;
            return Some(if neg {
                (v | !((1u64 << self.width) - 1)) as i64
            } else {
                v as i64
            });
        }
        // Wider values must be the sign (or zero) extension of their low 64 bits.
        let low = self.val[0];
        if self.resize(64, false).resize(self.width, signed) != *self {
            return None;
        }
        if !signed && low >> 63 == 1 {
            return None;
        }
        Some(low as i64)
    }

    /// True if any bit is a known 1; false if all are known 0; `None` otherwise.
    pub fn truth(&self) -> Option<bool> {
        let any_one = (0..self.val.len()).any(|i| self.val[i] & !self.unk(i) != 0);
        if any_one {
            Some(true)
        } else if self.has_unknown() {
            None
        } else {
            Some(false)
        }
    }

    pub fn is_zero(&self) -> bool {
        self.truth() == Some(false)
    }

    /// Change width, sign-extending (copying the top bit, including X/Z) if `signed`.
    pub fn resize(&self, width: u32, signed: bool) -> Bits {
        let mut b = Bits::zero(width);
        let (top_v, top_u) = self.msb();
        let n = words(width);
        let fill_v = if signed && top_v { u64::MAX } else { 0 };
        let fill_u = if signed && top_u { u64::MAX } else { 0 };
        let mut unk = vec![0; n];
        for (i, (v, u)) in b.val.iter_mut().zip(unk.iter_mut()).enumerate() {
            (*v, *u) = if i < self.val.len() {
                (self.val[i], self.unk(i))
            } else {
                (fill_v, fill_u)
            };
        }
        // Extend within the source's top word.
        if width > self.width && signed {
            for i in self.width..width.min(words(self.width) as u32 * 64) {
                let (w, bit) = ((i / 64) as usize, i % 64);
                b.val[w] = (b.val[w] & !(1 << bit)) | ((top_v as u64) << bit);
                unk[w] = (unk[w] & !(1 << bit)) | ((top_u as u64) << bit);
            }
        }
        b.unknown = Some(unk);
        b.mask();
        b
    }

    /// `width` bits starting at bit `lsb`. Bits outside the value read as X
    /// (or 0 if `four_state` is false).
    pub fn select(&self, lsb: i64, width: u32, four_state: bool) -> Bits {
        let mut b = Bits::zero(width);
        for i in 0..width {
            let src = lsb + i as i64;
            if src >= 0 && src < self.width as i64 {
                let (v, u) = self.bit(src as u32);
                b.set_bit(i, v, u);
            } else if four_state {
                b.set_bit(i, false, true);
            }
        }
        b.mask();
        b
    }

    /// Replace `width` bits at `lsb` with `part`. Out-of-range bits are ignored.
    pub fn insert(&mut self, lsb: i64, part: &Bits) {
        for i in 0..part.width {
            let dst = lsb + i as i64;
            if dst >= 0 && dst < self.width as i64 {
                let (v, u) = part.bit(i);
                self.set_bit(dst as u32, v, u);
            }
        }
        self.mask();
    }

    /// Concatenate, most significant part first.
    pub fn concat(parts: &[Bits]) -> Bits {
        let width: u32 = parts.iter().map(|p| p.width).sum();
        let mut b = Bits::zero(width.max(1));
        let mut at = width as i64;
        for p in parts {
            at -= p.width as i64;
            b.insert(at, p);
        }
        b
    }

    pub fn repl(&self, n: u32) -> Bits {
        Bits::concat(&vec![self.clone(); n as usize])
    }

    // ------------------------------------------------------------ bitwise

    /// Masks of known-0 and known-1 bits in word `i`.
    fn known(&self, i: usize) -> (u64, u64) {
        let u = self.unk(i);
        (!u & !self.val[i], !u & self.val[i])
    }

    fn from_known(width: u32, zero: Vec<u64>, one: Vec<u64>) -> Bits {
        let unknown: Vec<u64> = zero.iter().zip(&one).map(|(z, o)| !(z | o)).collect();
        let mut b = Bits {
            width,
            val: one,
            unknown: Some(unknown),
        };
        b.mask();
        b
    }

    pub fn and(&self, o: &Bits) -> Bits {
        let n = words(self.width);
        let (z, one) = (0..n)
            .map(|i| {
                let ((a0, a1), (b0, b1)) = (self.known(i), o.known(i));
                (a0 | b0, a1 & b1)
            })
            .unzip();
        Bits::from_known(self.width, z, one)
    }

    pub fn or(&self, o: &Bits) -> Bits {
        let n = words(self.width);
        let (z, one) = (0..n)
            .map(|i| {
                let ((a0, a1), (b0, b1)) = (self.known(i), o.known(i));
                (a0 & b0, a1 | b1)
            })
            .unzip();
        Bits::from_known(self.width, z, one)
    }

    pub fn xor(&self, o: &Bits) -> Bits {
        let n = words(self.width);
        let (z, one) = (0..n)
            .map(|i| {
                let ((a0, a1), (b0, b1)) = (self.known(i), o.known(i));
                ((a0 & b0) | (a1 & b1), (a0 & b1) | (a1 & b0))
            })
            .unzip();
        Bits::from_known(self.width, z, one)
    }

    pub fn not(&self) -> Bits {
        let n = words(self.width);
        let (z, one) = (0..n)
            .map(|i| {
                let (a0, a1) = self.known(i);
                (a1, a0)
            })
            .unzip();
        Bits::from_known(self.width, z, one)
    }

    pub fn xnor(&self, o: &Bits) -> Bits {
        self.xor(o).not()
    }

    /// Reduction: `&`, `|` or `^` over all bits, giving one bit.
    pub fn reduce_and(&self) -> Bits {
        let mut r = Bits::from_bool(true);
        for i in 0..self.width {
            r = r.and(&self.select(i as i64, 1, true));
        }
        r
    }

    pub fn reduce_or(&self) -> Bits {
        match self.truth() {
            Some(t) => Bits::from_bool(t),
            None => Bits::all_x(1),
        }
    }

    pub fn reduce_xor(&self) -> Bits {
        if self.has_unknown() {
            return Bits::all_x(1);
        }
        Bits::from_bool(self.val.iter().map(|w| w.count_ones()).sum::<u32>() % 2 == 1)
    }

    // ------------------------------------------------------------ arithmetic

    pub fn add(&self, o: &Bits) -> Bits {
        if self.has_unknown() || o.has_unknown() {
            return Bits::all_x(self.width);
        }
        let mut b = Bits::zero(self.width);
        let mut carry = 0u64;
        for i in 0..b.val.len() {
            let (s1, c1) = self.val[i].overflowing_add(o.val.get(i).copied().unwrap_or(0));
            let (s2, c2) = s1.overflowing_add(carry);
            b.val[i] = s2;
            carry = (c1 | c2) as u64;
        }
        b.mask();
        b
    }

    pub fn neg(&self) -> Bits {
        if self.has_unknown() {
            return Bits::all_x(self.width);
        }
        self.not().add(&Bits::from_u64(self.width, 1))
    }

    pub fn sub(&self, o: &Bits) -> Bits {
        if self.has_unknown() || o.has_unknown() {
            return Bits::all_x(self.width);
        }
        self.add(&o.neg())
    }

    pub fn mul(&self, o: &Bits) -> Bits {
        if self.has_unknown() || o.has_unknown() {
            return Bits::all_x(self.width);
        }
        let n = self.val.len();
        let mut r = vec![0u64; n];
        for i in 0..n {
            let mut carry = 0u128;
            for j in 0..n - i {
                let cur = r[i + j] as u128
                    + self.val[i] as u128 * o.val.get(j).copied().unwrap_or(0) as u128
                    + carry;
                r[i + j] = cur as u64;
                carry = cur >> 64;
            }
        }
        let mut b = Bits {
            width: self.width,
            val: r,
            unknown: None,
        };
        b.mask();
        b
    }

    /// Unsigned compare of known values of equal width.
    fn cmp_unsigned(&self, o: &Bits) -> Ordering {
        for i in (0..self.val.len()).rev() {
            match self.val[i].cmp(&o.val[i]) {
                Ordering::Equal => continue,
                c => return c,
            }
        }
        Ordering::Equal
    }

    fn cmp_values(&self, o: &Bits, signed: bool) -> Ordering {
        if signed {
            match (self.msb().0, o.msb().0) {
                (true, false) => return Ordering::Less,
                (false, true) => return Ordering::Greater,
                _ => {}
            }
        }
        self.cmp_unsigned(o)
    }

    /// Unsigned division and remainder of known values of equal width.
    fn divmod_unsigned(&self, o: &Bits) -> (Bits, Bits) {
        if self.width <= 128 {
            let a = self.val[0] as u128 | (self.val.get(1).copied().unwrap_or(0) as u128) << 64;
            let b = o.val[0] as u128 | (o.val.get(1).copied().unwrap_or(0) as u128) << 64;
            let (q, r) = (a / b, a % b);
            let mk = |x: u128| {
                let mut v = Bits::zero(self.width);
                v.val[0] = x as u64;
                if v.val.len() > 1 {
                    v.val[1] = (x >> 64) as u64;
                }
                v.mask();
                v
            };
            return (mk(q), mk(r));
        }
        let mut q = Bits::zero(self.width);
        let mut r = Bits::zero(self.width);
        for i in (0..self.width).rev() {
            r = r.shl(1);
            r.set_bit(0, self.bit(i).0, false);
            if r.cmp_unsigned(o) != Ordering::Less {
                r = r.sub(o);
                q.set_bit(i, true, false);
            }
        }
        (q, r)
    }

    /// `/` or `%`. Division by zero gives X.
    pub fn div_rem(&self, o: &Bits, signed: bool, rem: bool) -> Bits {
        if self.has_unknown() || o.has_unknown() || o.is_zero() {
            return Bits::all_x(self.width);
        }
        let (an, bn) = (signed && self.msb().0, signed && o.msb().0);
        let a = if an { self.neg() } else { self.clone() };
        let b = if bn { o.neg() } else { o.clone() };
        let (q, r) = a.divmod_unsigned(&b);
        if rem {
            if an { r.neg() } else { r }
        } else if an != bn {
            q.neg()
        } else {
            q
        }
    }

    /// `**` for integral operands.
    pub fn pow(&self, e: &Bits, signed_base: bool, signed_exp: bool) -> Bits {
        if self.has_unknown() || e.has_unknown() {
            return Bits::all_x(self.width);
        }
        let one = Bits::from_u64(self.width, 1);
        if signed_exp && e.msb().0 {
            // Negative exponent (LRM Table 11-4).
            let minus_one = Bits::ones(self.width);
            return if *self == one {
                one
            } else if signed_base && *self == minus_one {
                if e.val[0] & 1 == 1 { minus_one } else { one }
            } else if self.is_zero() {
                Bits::all_x(self.width)
            } else {
                Bits::zero(self.width)
            };
        }
        let mut result = one;
        let mut base = self.clone();
        for i in 0..e.width {
            if e.bit(i).0 {
                result = result.mul(&base);
            }
            base = base.mul(&base);
        }
        result
    }

    pub fn shl(&self, n: u64) -> Bits {
        let mut b = Bits::zero(self.width);
        for i in 0..self.width as u64 {
            if i >= n {
                let (v, u) = self.bit((i - n) as u32);
                b.set_bit(i as u32, v, u);
            }
        }
        b.mask();
        b
    }

    /// Logical (`signed` false) or arithmetic shift right.
    pub fn shr(&self, n: u64, arith: bool) -> Bits {
        let (tv, tu) = if arith { self.msb() } else { (false, false) };
        let mut b = Bits::zero(self.width);
        for i in 0..self.width as u64 {
            let src = i + n;
            let (v, u) = if src < self.width as u64 {
                self.bit(src as u32)
            } else {
                (tv, tu)
            };
            b.set_bit(i as u32, v, u);
        }
        b.mask();
        b
    }

    /// A shift by an amount held in `amount`. An unknown amount gives X.
    pub fn shift(&self, amount: &Bits, kind: Shift) -> Bits {
        if amount.has_unknown() {
            return Bits::all_x(self.width);
        }
        let big = amount.val[1..].iter().any(|&w| w != 0);
        let n = if big { u64::MAX } else { amount.val[0] };
        match kind {
            Shift::Left => self.shl(n),
            Shift::Right => self.shr(n, false),
            Shift::Arith => self.shr(n, true),
        }
    }

    // ------------------------------------------------------------ comparison

    /// `==`: X if the result depends on unknown bits.
    pub fn logic_eq(&self, o: &Bits) -> Bits {
        // Any known bit that differs decides "not equal".
        for i in 0..self.val.len() {
            let ((a0, a1), (b0, b1)) = (self.known(i), o.known(i));
            if (a0 & b1) | (a1 & b0) != 0 {
                return Bits::from_bool(false);
            }
        }
        if self.has_unknown() || o.has_unknown() {
            Bits::all_x(1)
        } else {
            Bits::from_bool(true)
        }
    }

    /// `===`: exact comparison, including X and Z.
    pub fn case_eq(&self, o: &Bits) -> bool {
        self.val == o.val && self.unknown == o.unknown
    }

    /// `==?`: X and Z bits in `o` match anything.
    pub fn wild_eq(&self, o: &Bits) -> Bits {
        let mut a = self.clone();
        let mut b = o.clone();
        if let Some(u) = o.unknown.clone() {
            for (i, m) in u.iter().enumerate() {
                a.val[i] &= !m;
                b.val[i] &= !m;
                if let Some(au) = &mut a.unknown {
                    au[i] &= !m;
                }
            }
            b.unknown = None;
            a.mask();
        }
        a.logic_eq(&b)
    }

    /// `casez` (`x_wild` false) or `casex` match.
    pub fn case_match(&self, o: &Bits, x_wild: bool) -> bool {
        for i in 0..self.width {
            let ((av, au), (bv, bu)) = (self.bit(i), o.bit(i));
            let a_wild = au && (av || x_wild);
            let b_wild = bu && (bv || x_wild);
            if a_wild || b_wild {
                continue;
            }
            if (av, au) != (bv, bu) {
                return false;
            }
        }
        true
    }

    /// `<`, `<=`, `>`, `>=`.
    pub fn relational(&self, o: &Bits, signed: bool, op: Rel) -> Bits {
        if self.has_unknown() || o.has_unknown() {
            return Bits::all_x(1);
        }
        let c = self.cmp_values(o, signed);
        Bits::from_bool(match op {
            Rel::Lt => c == Ordering::Less,
            Rel::Le => c != Ordering::Greater,
            Rel::Gt => c == Ordering::Greater,
            Rel::Ge => c != Ordering::Less,
        })
    }

    pub fn count_ones(&self) -> u32 {
        (0..self.val.len())
            .map(|i| (self.val[i] & !self.unk(i)).count_ones())
            .sum()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shift {
    Left,
    Right,
    Arith,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rel {
    Lt,
    Le,
    Gt,
    Ge,
}

/// A parsed numeric literal.
#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    /// An integral literal. `sized` is false for `12` and `'hFF`, which are at
    /// least 32 bits.
    Bits {
        bits: Bits,
        signed: bool,
        sized: bool,
    },
    /// `'0`, `'1`, `'x` or `'z`: fills whatever width the context needs.
    Fill(Bits),
    Real(f64),
    /// A time literal such as `10ns`: the value and the unit as a power of ten of seconds.
    Time(f64, i8),
}

/// Parse a literal token: `12`, `8'hFF`, `'sb1x?`, `'1`, `1.5e-3`, `10ns`.
pub fn parse_literal(text: &str) -> Result<Literal, String> {
    let t: String = text.chars().filter(|&c| c != '_').collect();
    for (suffix, exp) in [
        ("fs", -15),
        ("ps", -12),
        ("ns", -9),
        ("us", -6),
        ("ms", -3),
        ("s", 0),
    ] {
        if let Some(num) = t.strip_suffix(suffix)
            && !num.is_empty()
            && num.chars().all(|c| c.is_ascii_digit() || c == '.')
        {
            let v: f64 = num
                .parse()
                .map_err(|_| format!("bad time literal {text}"))?;
            return Ok(Literal::Time(v, exp));
        }
    }
    if t == "1step" {
        return Ok(Literal::Time(1.0, -100));
    }
    let Some(apos) = t.find('\'') else {
        if t.contains(['.', 'e', 'E']) {
            return t
                .parse::<f64>()
                .map(Literal::Real)
                .map_err(|_| format!("bad real literal {text}"));
        }
        return decimal(&t, None, true).map(|bits| Literal::Bits {
            bits,
            signed: true,
            sized: false,
        });
    };
    let (size, rest) = (&t[..apos], &t[apos + 1..]);
    let size: Option<u32> = if size.is_empty() {
        None
    } else {
        Some(size.parse().map_err(|_| format!("bad size in {text}"))?).filter(|&s| s > 0)
    };
    if size.is_none() && rest.len() == 1 {
        let b = match rest {
            "0" => Bits::zero(1),
            "1" => Bits::ones(1),
            "x" | "X" => Bits::all_x(1),
            "z" | "Z" | "?" => Bits::all_z(1),
            _ => return Err(format!("bad literal {text}")),
        };
        return Ok(Literal::Fill(b));
    }
    let mut chars = rest.chars();
    let mut signed = false;
    let mut base = chars.next().ok_or(format!("bad literal {text}"))?;
    if base == 's' || base == 'S' {
        signed = true;
        base = chars.next().ok_or(format!("bad literal {text}"))?;
    }
    let digits: String = chars.collect();
    if digits.is_empty() {
        return Err(format!("missing digits in {text}"));
    }
    let bits_per = match base.to_ascii_lowercase() {
        'b' => 1,
        'o' => 3,
        'h' => 4,
        'd' => {
            let bits = decimal(&digits, size, false)?;
            return Ok(Literal::Bits {
                bits,
                signed,
                sized: size.is_some(),
            });
        }
        _ => return Err(format!("bad base in {text}")),
    };
    let natural = (digits.len() as u32 * bits_per).max(1);
    let mut b = Bits::zero(natural);
    for (k, c) in digits.chars().rev().enumerate() {
        let at = k as u32 * bits_per;
        let (v, u): (u32, bool) = match c.to_ascii_lowercase() {
            'x' => (0, true),
            'z' | '?' => ((1 << bits_per) - 1, true),
            d => {
                let v = d
                    .to_digit(1 << bits_per)
                    .ok_or(format!("bad digit '{d}' in {text}"))?;
                (v, false)
            }
        };
        for j in 0..bits_per {
            let bit_v = if u {
                (v >> j) & 1 == 1 && c != 'x' && c != 'X'
            } else {
                (v >> j) & 1 == 1
            };
            b.set_bit(at + j, bit_v, u);
        }
    }
    b.mask();
    let width = size.unwrap_or(natural.max(32));
    // Extend with X or Z when the leftmost digit is X or Z, otherwise with 0.
    let (lv, lu) = b.msb();
    let mut r = b.resize(width, false);
    if lu && width > natural {
        for i in natural..width {
            r.set_bit(i, lv, true);
        }
    }
    r.mask();
    Ok(Literal::Bits {
        bits: r,
        signed,
        sized: size.is_some(),
    })
}

/// A decimal number of `size` bits (or at least 32 if unsized).
fn decimal(digits: &str, size: Option<u32>, plain: bool) -> Result<Bits, String> {
    let lower = digits.to_ascii_lowercase();
    if !plain && (lower == "x" || lower == "z" || lower == "?") {
        let w = size.unwrap_or(32);
        return Ok(if lower == "x" {
            Bits::all_x(w)
        } else {
            Bits::all_z(w)
        });
    }
    // Accumulate in 64-bit words: value = value * 10 + digit.
    let mut v = Bits::zero(64);
    let ten = Bits::from_u64(64, 10);
    for c in digits.chars() {
        let d = c.to_digit(10).ok_or(format!("bad decimal digit '{c}'"))?;
        // Grow before value * 10 + digit could overflow.
        if v.width - v.leading_zeros() + 4 > v.width {
            v = v.resize(v.width + 64, false);
        }
        v = v
            .mul(&ten.resize(v.width, false))
            .add(&Bits::from_u64(v.width, d as u64));
    }
    let natural = v.width - v.leading_zeros();
    let width = size.unwrap_or(natural.max(32));
    Ok(v.resize(width.max(1), false))
}

impl Bits {
    fn leading_zeros(&self) -> u32 {
        for i in (0..self.width).rev() {
            if self.bit(i) != (false, false) {
                return self.width - 1 - i;
            }
        }
        self.width
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(s: &str) -> Bits {
        match parse_literal(s).unwrap() {
            Literal::Bits { bits, .. } | Literal::Fill(bits) => bits,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn literals() {
        assert_eq!(lit("8'hFF"), Bits::from_u64(8, 255));
        assert_eq!(lit("12"), Bits::from_u64(32, 12));
        assert_eq!(lit("4'b10_01"), Bits::from_u64(4, 9));
        assert_eq!(lit("'hFF").width, 32);
        assert_eq!(lit("64'h5aef0c8d_d70a4497").to_u64(), 0x5aef0c8dd70a4497);
        assert_eq!(lit("68'h1_0000_0000_0000_0001").val, vec![1, 1]);
        assert_eq!(lit("123456789012345678901234567890").width, 97);
        let x = lit("4'bx01z");
        assert_eq!(x.bit(3), (false, true));
        assert_eq!(x.bit(2), (false, false));
        assert_eq!(x.bit(1), (true, false));
        assert_eq!(x.bit(0), (true, true));
        assert_eq!(lit("8'hx"), Bits::all_x(8));
        assert_eq!(lit("8'dz"), Bits::all_z(8));
        assert!(matches!(parse_literal("'1"), Ok(Literal::Fill(_))));
        assert!(matches!(parse_literal("1.5e3"), Ok(Literal::Real(v)) if v == 1500.0));
        assert!(matches!(parse_literal("10ns"), Ok(Literal::Time(v, -9)) if v == 10.0));
        assert!(matches!(
            parse_literal("'sd5"),
            Ok(Literal::Bits { signed: true, .. })
        ));
    }

    #[test]
    fn arithmetic_wraps() {
        let a = Bits::from_u64(8, 200);
        let b = Bits::from_u64(8, 100);
        assert_eq!(a.add(&b), Bits::from_u64(8, 44));
        assert_eq!(b.sub(&a), Bits::from_u64(8, 156));
        assert_eq!(a.mul(&b), Bits::from_u64(8, (200u32 * 100 % 256) as u64));
        let wide = Bits::ones(100).add(&Bits::from_u64(100, 1));
        assert!(wide.is_zero());
    }

    #[test]
    fn signed_division_and_compare() {
        let m7 = Bits::from_i64(8, -7);
        let two = Bits::from_u64(8, 2);
        assert_eq!(m7.div_rem(&two, true, false).to_i64(true), Some(-3));
        assert_eq!(m7.div_rem(&two, true, true).to_i64(true), Some(-1));
        assert_eq!(m7.relational(&two, true, Rel::Lt), Bits::from_bool(true));
        assert_eq!(m7.relational(&two, false, Rel::Lt), Bits::from_bool(false));
        assert_eq!(two.div_rem(&Bits::zero(8), false, false), Bits::all_x(8));
    }

    #[test]
    fn four_state_logic() {
        let x = Bits::all_x(1);
        let zero = Bits::from_bool(false);
        let one = Bits::from_bool(true);
        assert_eq!(x.and(&zero), zero);
        assert_eq!(x.or(&one), one);
        assert_eq!(x.and(&one), Bits::all_x(1));
        assert_eq!(x.add(&one), Bits::all_x(1));
        assert_eq!(
            lit("4'b10x0").logic_eq(&lit("4'b0000")),
            Bits::from_bool(false)
        );
        assert_eq!(lit("4'b10x0").logic_eq(&lit("4'b1000")), Bits::all_x(1));
        assert!(lit("4'b10x0").case_eq(&lit("4'b10x0")));
        assert!(lit("4'b1010").case_match(&lit("4'b1?1?"), false));
        assert!(!lit("4'b1010").case_match(&lit("4'b0?1?"), false));
        assert!(lit("4'b10x0").case_match(&lit("4'b1000"), true));
        assert_eq!(
            lit("4'b1010").wild_eq(&lit("4'b1x1x")),
            Bits::from_bool(true)
        );
    }

    #[test]
    fn resize_select_concat_shift() {
        let m1 = Bits::from_i64(4, -1);
        assert_eq!(m1.resize(8, true), Bits::from_u64(8, 0xff));
        assert_eq!(m1.resize(8, false), Bits::from_u64(8, 0x0f));
        assert_eq!(
            Bits::from_u64(8, 0xab).resize(4, false),
            Bits::from_u64(4, 0xb)
        );
        assert_eq!(
            Bits::from_u64(8, 0xab).select(4, 4, true),
            Bits::from_u64(4, 0xa)
        );
        assert_eq!(
            Bits::from_u64(8, 0xab).select(6, 4, true).bit(3),
            (false, true)
        );
        let c = Bits::concat(&[Bits::from_u64(4, 0xa), Bits::from_u64(8, 0xbc)]);
        assert_eq!(c, Bits::from_u64(12, 0xabc));
        assert_eq!(Bits::from_u64(2, 2).repl(3), Bits::from_u64(6, 0b101010));
        assert_eq!(
            Bits::from_u64(8, 0x81).shr(1, true),
            Bits::from_u64(8, 0xc0)
        );
        assert_eq!(Bits::from_u64(8, 0x81).shl(1), Bits::from_u64(8, 0x02));
        assert_eq!(
            Bits::from_u64(8, 3).pow(&Bits::from_u64(8, 4), false, false),
            Bits::from_u64(8, 81)
        );
    }
}
