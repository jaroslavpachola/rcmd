//! The command line's calculator: `= 2*(3+4)`, `= 0x1f + 1`,
//! `= 3G / 4K`. A small recursive-descent parser over one expression -
//! integers while they stay integers, floats once a division or a
//! decimal point makes them, and the size suffixes a file manager talks
//! in.
//!
//! Grammar, loosest first:
//!
//! ```text
//! expr   = term (('+' | '-') term)*
//! term   = unary (('*' | '/' | '%') unary)*
//! unary  = ('-' | '+') unary | power    so -2**2 is -(2**2)
//! power  = atom ('**' unary)?           right-associative
//! atom   = number suffix? | '(' expr ')'
//! ```

/// A result: exact while it can be, a float once it cannot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    Int(i128),
    Float(f64),
}

impl Value {
    fn float(self) -> f64 {
        match self {
            Value::Int(n) => n as f64,
            Value::Float(f) => f,
        }
    }
}

impl std::fmt::Display for Value {
    /// An integer as it is, and in hex beside it when that says
    /// something; a float to as many places as it needs, up to twelve.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Value::Int(n) if n.unsigned_abs() >= 10 => write!(f, "{n}  (0x{n:x})"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(x) if x.is_finite() && x == x.trunc() && x.abs() < 1e15 => {
                write!(f, "{}", x as i128)
            }
            Value::Float(x) => {
                let text = format!("{x:.12}");
                let text = text.trim_end_matches('0').trim_end_matches('.');
                write!(f, "{text}")
            }
        }
    }
}

/// Work out `text`, or say where it stopped making sense.
pub fn eval(text: &str) -> Result<Value, String> {
    let mut parser = Parser {
        chars: text.chars().collect(),
        at: 0,
    };
    let value = parser.expr()?;
    parser.blank();
    if parser.at < parser.chars.len() {
        return Err(format!("unexpected '{}'", parser.chars[parser.at]));
    }
    Ok(value)
}

struct Parser {
    chars: Vec<char>,
    at: usize,
}

impl Parser {
    fn blank(&mut self) {
        while self.chars.get(self.at).is_some_and(|c| c.is_whitespace()) {
            self.at += 1;
        }
    }

    /// The next character past blanks, taken if it is `want`.
    fn eat(&mut self, want: &str) -> bool {
        self.blank();
        let want: Vec<char> = want.chars().collect();
        if self.chars[self.at..].starts_with(&want) {
            self.at += want.len();
            true
        } else {
            false
        }
    }

    fn expr(&mut self) -> Result<Value, String> {
        let mut value = self.term()?;
        loop {
            if self.eat("+") {
                value = arith(value, self.term()?, '+')?;
            } else if self.eat("-") {
                value = arith(value, self.term()?, '-')?;
            } else {
                return Ok(value);
            }
        }
    }

    fn term(&mut self) -> Result<Value, String> {
        let mut value = self.unary()?;
        loop {
            // `**` is a power, not two products
            self.blank();
            if self.chars[self.at..].starts_with(&['*', '*']) {
                return Ok(value);
            }
            let op = if self.eat("*") {
                '*'
            } else if self.eat("/") {
                '/'
            } else if self.eat("%") {
                '%'
            } else {
                return Ok(value);
            };
            value = arith(value, self.unary()?, op)?;
        }
    }

    fn power(&mut self) -> Result<Value, String> {
        let base = self.atom()?;
        if self.eat("**") {
            let exponent = self.unary()?;
            return arith(base, exponent, '^');
        }
        Ok(base)
    }

    fn unary(&mut self) -> Result<Value, String> {
        if self.eat("-") {
            return arith(Value::Int(0), self.unary()?, '-');
        }
        if self.eat("+") {
            return self.unary();
        }
        self.power()
    }

    fn atom(&mut self) -> Result<Value, String> {
        if self.eat("(") {
            let value = self.expr()?;
            if !self.eat(")") {
                return Err("a '(' is not closed".into());
            }
            return Ok(value);
        }
        self.blank();
        let start = self.at;
        let radix = match self.chars.get(self.at..self.at + 2) {
            Some(['0', 'x' | 'X']) => 16,
            Some(['0', 'o' | 'O']) => 8,
            Some(['0', 'b' | 'B']) => 2,
            _ => 10,
        };
        if radix != 10 {
            self.at += 2;
            let digits: String = self.take(|c| c.is_ascii_hexdigit() || c == '_');
            let digits = digits.replace('_', "");
            let n = i128::from_str_radix(&digits, radix)
                .map_err(|_| format!("not a number in base {radix}: {digits}"))?;
            return Ok(Value::Int(n));
        }
        let digits: String = self.take(|c| c.is_ascii_digit() || c == '.' || c == '_');
        if digits.is_empty() {
            return Err(match self.chars.get(start) {
                Some(c) => format!("unexpected '{c}'"),
                None => "the expression ends too soon".into(),
            });
        }
        let digits = digits.replace('_', "");
        let value = match digits.contains('.') {
            true => Value::Float(
                digits
                    .parse()
                    .map_err(|_| format!("not a number: {digits}"))?,
            ),
            false => Value::Int(digits.parse().map_err(|_| format!("too big: {digits}"))?),
        };
        // a size suffix, binary as a file manager's sizes are
        let scale: Option<i128> = match self.chars.get(self.at) {
            Some('k' | 'K') => Some(1 << 10),
            Some('m' | 'M') => Some(1 << 20),
            Some('g' | 'G') => Some(1 << 30),
            Some('t' | 'T') => Some(1 << 40),
            _ => None,
        };
        match scale {
            Some(scale) => {
                self.at += 1;
                arith(value, Value::Int(scale), '*')
            }
            None => Ok(value),
        }
    }

    fn take(&mut self, keep: impl Fn(char) -> bool) -> String {
        let start = self.at;
        while self.chars.get(self.at).is_some_and(|&c| keep(c)) {
            self.at += 1;
        }
        self.chars[start..self.at].iter().collect()
    }
}

/// One operation: integers stay integers where the answer is one, and
/// overflow or a division that does not come out even goes to floats.
fn arith(a: Value, b: Value, op: char) -> Result<Value, String> {
    if let (Value::Int(x), Value::Int(y)) = (a, b) {
        let exact = match op {
            '+' => x.checked_add(y),
            '-' => x.checked_sub(y),
            '*' => x.checked_mul(y),
            '/' if y == 0 => return Err("division by zero".into()),
            '/' if x % y == 0 => Some(x / y),
            '%' if y == 0 => return Err("division by zero".into()),
            '%' => Some(x % y),
            '^' if (0..=127).contains(&y) => x.checked_pow(y as u32),
            _ => None,
        };
        if let Some(n) = exact {
            return Ok(Value::Int(n));
        }
    }
    let (x, y) = (a.float(), b.float());
    let result = match op {
        '+' => x + y,
        '-' => x - y,
        '*' => x * y,
        '/' if y == 0.0 => return Err("division by zero".into()),
        '/' => x / y,
        '%' if y == 0.0 => return Err("division by zero".into()),
        '%' => x % y,
        _ => x.powf(y),
    };
    Ok(Value::Float(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(text: &str) -> String {
        match eval(text) {
            Ok(value) => value.to_string(),
            Err(err) => format!("error: {err}"),
        }
    }

    #[test]
    fn arithmetic_the_way_it_is_written() {
        assert_eq!(show("2*(3+4)"), "14  (0xe)");
        assert_eq!(show("1 + 2 * 3"), "7");
        assert_eq!(show("2 ** 3 ** 2"), "512  (0x200)");
        assert_eq!(show("-2 ** 2"), "-4");
        assert_eq!(show("2 ** -1"), "0.5");
        assert_eq!(show("7 / 2"), "3.5");
        assert_eq!(show("8 / 2"), "4");
        assert_eq!(show("7 % 3"), "1");
        assert_eq!(show("0.1 + 0.2"), "0.3");
        assert_eq!(show("1.5 * 2"), "3");
    }

    #[test]
    fn bases_and_sizes() {
        assert_eq!(show("0x1f + 1"), "32  (0x20)");
        assert_eq!(show("0b1010"), "10  (0xa)");
        assert_eq!(show("0o17"), "15  (0xf)");
        assert_eq!(show("3G / 4K"), "786432  (0xc0000)");
        assert_eq!(show("1_000_000"), "1000000  (0xf4240)");
    }

    #[test]
    fn nonsense_says_where() {
        assert_eq!(show("1 / 0"), "error: division by zero");
        assert_eq!(show("(1 + 2"), "error: a '(' is not closed");
        assert_eq!(show("1 +"), "error: the expression ends too soon");
        assert_eq!(show("2 x 3"), "error: unexpected 'x'");
        // too big for an integer becomes a float rather than wrapping
        assert!(show("2 ** 200").starts_with("1606938044258990"));
    }
}
