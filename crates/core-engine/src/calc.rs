//! Instant zero-allocation recursive-descent math calculator (inspired by MIT-licensed Nanolight).
//!
//! Evaluates `+ - * / % ^`, Unicode `× ÷ −`, parentheses, scientific notation, constants (`pi`/`π`, `e`),
//! and functions (`sqrt`, `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `ln`, `log`, `log2`, `abs`,
//! `floor`, `ceil`, `round`).
//!
//! Only triggers when at least one operator or math function is present so bare numbers (`"42"`) or
//! single constants (`"e"`, `"pi"`) remain normal searches.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalcAnswer {
    pub expression: String,
    pub formatted: String,
    pub raw_value: f64,
    #[serde(default)]
    pub hex: Option<String>,
}

pub fn eval_math_query(input: &str) -> Option<CalcAnswer> {
    let trimmed = input.trim().trim_end_matches('=').trim();
    if trimmed.is_empty() || trimmed.len() > 256 {
        return None;
    }

    let mut parser = Parser {
        s: trimmed,
        i: 0,
        saw_math_op_or_fn: false,
    };

    let val = parser.parse_expr().ok()?;
    parser.skip_ws();
    if parser.i != parser.s.len() || !parser.saw_math_op_or_fn || !val.is_finite() {
        return None;
    }

    // Normalize -0.0 to 0.0
    let clean_val = if val == 0.0 { 0.0 } else { val };
    let formatted = format_number_grouped(clean_val);
    let hex = if clean_val >= 0.0 && clean_val < 1e15 && clean_val == clean_val.round() {
        Some(format!("0x{:X}", clean_val as u64))
    } else {
        None
    };

    Some(CalcAnswer {
        expression: trimmed.to_string(),
        formatted,
        raw_value: clean_val,
        hex,
    })
}

struct Parser<'a> {
    s: &'a str,
    i: usize,
    saw_math_op_or_fn: bool,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.s.len() {
            let b = self.s.as_bytes()[self.i];
            if b == b' ' || b == b'\t' {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    fn peek_byte(&mut self) -> u8 {
        self.skip_ws();
        if self.i < self.s.len() {
            self.s.as_bytes()[self.i]
        } else {
            0
        }
    }

    fn consume_op(&mut self, ascii: u8, utf8_alt: &str) -> bool {
        self.skip_ws();
        if self.i < self.s.len() && self.s.as_bytes()[self.i] == ascii {
            self.i += 1;
            return true;
        }
        if !utf8_alt.is_empty() && self.s[self.i..].starts_with(utf8_alt) {
            self.i += utf8_alt.len();
            return true;
        }
        false
    }

    fn parse_expr(&mut self) -> Result<f64, ()> {
        let mut v = self.parse_term()?;
        loop {
            if self.consume_op(b'+', "") {
                self.saw_math_op_or_fn = true;
                v += self.parse_term()?;
            } else if self.consume_op(b'-', "−") {
                self.saw_math_op_or_fn = true;
                v -= self.parse_term()?;
            } else {
                return Ok(v);
            }
        }
    }

    fn parse_term(&mut self) -> Result<f64, ()> {
        let mut v = self.parse_power()?;
        loop {
            if self.consume_op(b'*', "×") {
                self.saw_math_op_or_fn = true;
                v *= self.parse_power()?;
            } else if self.consume_op(b'/', "÷") {
                self.saw_math_op_or_fn = true;
                let denom = self.parse_power()?;
                if denom == 0.0 {
                    return Err(());
                }
                v /= denom;
            } else if self.consume_op(b'%', "") {
                self.saw_math_op_or_fn = true;
                let denom = self.parse_power()?;
                if denom == 0.0 {
                    return Err(());
                }
                v %= denom;
            } else {
                return Ok(v);
            }
        }
    }

    /// Right-associative exponentiation (`2^3^2 == 512`).
    fn parse_power(&mut self) -> Result<f64, ()> {
        let base = self.parse_unary()?;
        if self.consume_op(b'^', "") {
            self.saw_math_op_or_fn = true;
            let exp = self.parse_power()?;
            return Ok(base.powf(exp));
        }
        Ok(base)
    }

    fn parse_unary(&mut self) -> Result<f64, ()> {
        if self.consume_op(b'-', "−") {
            return Ok(-self.parse_unary()?);
        }
        if self.consume_op(b'+', "") {
            return self.parse_unary();
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<f64, ()> {
        self.skip_ws();
        if self.i >= self.s.len() {
            return Err(());
        }
        if self.s[self.i..].starts_with('π') {
            self.i += 'π'.len_utf8();
            return Ok(std::f64::consts::PI);
        }
        let ch = self.peek_byte();
        if ch == b'(' {
            self.i += 1;
            let v = self.parse_expr()?;
            if !self.consume_op(b')', "") {
                return Err(());
            }
            return Ok(v);
        }
        if ch == b'0'
            && self.i + 2 <= self.s.len()
            && (self.s.as_bytes().get(self.i + 1) == Some(&b'x')
                || self.s.as_bytes().get(self.i + 1) == Some(&b'X'))
        {
            return self.parse_hex_literal();
        }
        if ch.is_ascii_digit() || ch == b'.' {
            return self.parse_number();
        }
        if ch.is_ascii_alphabetic() {
            return self.parse_ident_or_fn();
        }
        Err(())
    }

    fn parse_hex_literal(&mut self) -> Result<f64, ()> {
        self.i += 2; // skip 0x
        let start = self.i;
        while self.i < self.s.len() {
            let b = self.s.as_bytes()[self.i];
            if b.is_ascii_hexdigit() || b == b'_' {
                self.i += 1;
            } else {
                break;
            }
        }
        let clean: String = self.s[start..self.i].chars().filter(|&c| c != '_').collect();
        if clean.is_empty() {
            return Err(());
        }
        let v = u64::from_str_radix(&clean, 16).map_err(|_| ())?;
        Ok(v as f64)
    }

    fn parse_number(&mut self) -> Result<f64, ()> {
        let start = self.i;
        let bytes = self.s.as_bytes();
        while self.i < bytes.len()
            && (bytes[self.i].is_ascii_digit() || bytes[self.i] == b'.' || bytes[self.i] == b'_')
        {
            self.i += 1;
        }
        // Scientific notation: 1e3, 2.5E-4
        if self.i + 1 < bytes.len() && (bytes[self.i] == b'e' || bytes[self.i] == b'E') {
            let next = bytes[self.i + 1];
            let valid_sci = next.is_ascii_digit()
                || ((next == b'-' || next == b'+')
                    && self.i + 2 < bytes.len()
                    && bytes[self.i + 2].is_ascii_digit());
            if valid_sci {
                self.i += 2;
                while self.i < bytes.len() && bytes[self.i].is_ascii_digit() {
                    self.i += 1;
                }
            }
        }
        let raw = &self.s[start..self.i];
        if raw.contains('_') {
            let clean: String = raw.chars().filter(|&c| c != '_').collect();
            clean.parse::<f64>().map_err(|_| ())
        } else {
            raw.parse::<f64>().map_err(|_| ())
        }
    }

    fn parse_ident_or_fn(&mut self) -> Result<f64, ()> {
        let start = self.i;
        let bytes = self.s.as_bytes();
        while self.i < bytes.len()
            && (bytes[self.i].is_ascii_alphanumeric() || bytes[self.i] == b'_')
        {
            self.i += 1;
        }
        let name = self.s[start..self.i].to_ascii_lowercase();
        match name.as_str() {
            "pi" => return Ok(std::f64::consts::PI),
            "e" => return Ok(std::f64::consts::E),
            "tau" => return Ok(std::f64::consts::TAU),
            _ => {}
        }

        // Must be a recognized math function followed by `(expr)` or unary argument
        let arg = if self.peek_byte() == b'(' {
            self.i += 1;
            let v = self.parse_expr()?;
            if !self.consume_op(b')', "") {
                return Err(());
            }
            v
        } else {
            self.parse_unary()?
        };

        self.saw_math_op_or_fn = true;
        let out = match name.as_str() {
            "sqrt" => {
                if arg < 0.0 {
                    return Err(());
                }
                arg.sqrt()
            }
            "sin" => arg.sin(),
            "cos" => arg.cos(),
            "tan" => arg.tan(),
            "asin" => arg.asin(),
            "acos" => arg.acos(),
            "atan" => arg.atan(),
            "ln" => {
                if arg <= 0.0 {
                    return Err(());
                }
                arg.ln()
            }
            "log" | "log10" => {
                if arg <= 0.0 {
                    return Err(());
                }
                arg.log10()
            }
            "log2" => {
                if arg <= 0.0 {
                    return Err(());
                }
                arg.log2()
            }
            "abs" => arg.abs(),
            "floor" => arg.floor(),
            "ceil" => arg.ceil(),
            "round" => arg.round(),
            _ => return Err(()),
        };
        if out.is_finite() { Ok(out) } else { Err(()) }
    }
}

fn format_number_grouped(val: f64) -> String {
    let abs = val.abs();
    if abs != 0.0 && !(1e-6..1e15).contains(&abs) {
        return format!("{:e}", val);
    }

    // Round to 10 significant decimal places to strip IEEE-754 representation noise (e.g. 0.1 + 0.2 -> 0.3)
    let rounded = if (val - val.round()).abs() < 1e-11 && abs < 1e15 {
        val.round()
    } else {
        val
    };

    if rounded == rounded.trunc() && rounded.abs() < 1e15 {
        let n = rounded.abs() as u64;
        let grouped = group_u64_commas(n);
        return if val < 0.0 && n > 0 {
            format!("-{}", grouped)
        } else {
            grouped
        };
    }

    let raw = format!("{:.10}", rounded);
    let trimmed = raw.trim_end_matches('0').trim_end_matches('.');
    if let Some((int_part, frac_part)) = trimmed.split_once('.') {
        let neg = int_part.starts_with('-');
        let digits = int_part.trim_start_matches('-');
        if let Ok(n) = digits.parse::<u64>() {
            let grouped = group_u64_commas(n);
            return if neg {
                format!("-{}.{}", grouped, frac_part)
            } else {
                format!("{}.{}", grouped, frac_part)
            };
        }
    }
    trimmed.to_string()
}

fn group_u64_commas(mut n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    let mut parts = Vec::new();
    while n >= 1_000 {
        parts.push(format!("{:03}", n % 1_000));
        n /= 1_000;
    }
    parts.push(n.to_string());
    parts.reverse();
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nanolight_math_eval_and_guards() {
        // Bare numbers and constants stay searches!
        assert!(eval_math_query("42").is_none());
        assert!(eval_math_query("-42").is_none());
        assert!(eval_math_query("pi").is_none());
        assert!(eval_math_query("e").is_none());
        assert!(eval_math_query("q3 revenue").is_none());

        // Power + multiply + hex
        let ans = eval_math_query("2^10 * 3").expect("2^10 * 3");
        assert_eq!(ans.formatted, "3,072");
        assert_eq!(ans.hex.as_deref(), Some("0xC00"));

        // Right-associative power: 2^3^2 == 2^9 == 512
        let ans2 = eval_math_query("2^3^2").expect("2^3^2");
        assert_eq!(ans2.formatted, "512");
        assert_eq!(ans2.hex.as_deref(), Some("0x200"));

        // Unicode operators & functions
        let ans3 = eval_math_query("sqrt(144) × 10 ÷ 3").expect("unicode ops");
        assert_eq!(ans3.formatted, "40");

        // IEEE-754 clean decimal formatting
        let ans4 = eval_math_query("0.1 + 0.2").expect("0.1 + 0.2");
        assert_eq!(ans4.formatted, "0.3");
        assert!(ans4.hex.is_none());
    }
}
