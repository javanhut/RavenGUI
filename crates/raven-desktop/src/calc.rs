//! The launcher's inline calculator.
//!
//! "2+2" typed into a search field is a question, and the honest answer is
//! "4", not "no applications match". This is the smallest evaluator that
//! answers it: the four operations, parentheses, decimals and a unary minus,
//! over `f64`. No functions, no variables, no units — anything past this is a
//! calculator application, and there is a launcher for those.
//!
//! Pure and string-in, string-out, so the launcher never sees a float: what
//! it gets is the text of the row it draws, or `None` when the query is not
//! arithmetic at all. That includes a query that is *only* a number: "7" is
//! most likely the start of something else, and a row saying "= 7" under it
//! would be the launcher agreeing with the user for no reason.

/// The value of `query` as arithmetic, formatted for a result row, or `None`
/// if it is not an expression — a name, a typo, a bare number, a division by
/// zero, anything that would answer with nothing useful.
pub fn calculate(query: &str) -> Option<String> {
    let mut parser = Parser {
        chars: query.trim().chars().collect(),
        at: 0,
        computed: false,
    };
    if parser.chars.is_empty() {
        return None;
    }
    let value = parser.expression()?;
    if parser.at != parser.chars.len() || !parser.computed {
        return None;
    }
    // Division by zero, overflow, `0/0`: none of them is a number, and "= inf"
    // or "= NaN" is not an answer anyone typed the question for.
    if !value.is_finite() {
        return None;
    }
    Some(format_value(value))
}

/// `value` the way a person would write it: integers without a point, and
/// nothing after the point but the digits that mean something.
///
/// Formatted to ten places first because `0.1 + 0.2` is `0.30000000000000004`
/// in `f64`, and a calculator that says so is technically right and
/// practically wrong. Ten is enough for anything typed by hand and few enough
/// to hide the noise.
fn format_value(value: f64) -> String {
    let mut text = format!("{value:.10}");
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    // Rounding can leave "-0", which reads as a mistake rather than a value.
    if text == "-0" {
        text = "0".to_owned();
    }
    text
}

/// A recursive-descent parser over the grammar
///
/// ```text
/// expression := term   (('+' | '-') term)*
/// term       := factor (('*' | '/') factor)*
/// factor     := '-' factor | '(' expression ')' | number
/// ```
///
/// which is what gives `*` and `/` precedence over `+` and `-`, and makes
/// every operator associate left. Whitespace is skipped between tokens so
/// "2 + 2" and "2+2" are the same question.
struct Parser {
    chars: Vec<char>,
    at: usize,
    /// Whether anything was *done* — an operator applied or a parenthesis
    /// opened. A bare number parses but is not a calculation; see
    /// [`calculate`].
    computed: bool,
}

impl Parser {
    fn peek(&mut self) -> Option<char> {
        while self.chars.get(self.at).is_some_and(|c| c.is_whitespace()) {
            self.at += 1;
        }
        self.chars.get(self.at).copied()
    }

    fn take(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expression(&mut self) -> Option<f64> {
        let mut left = self.term()?;
        loop {
            if self.take('+') {
                left += self.term()?;
            } else if self.take('-') {
                left -= self.term()?;
            } else {
                return Some(left);
            }
            self.computed = true;
        }
    }

    fn term(&mut self) -> Option<f64> {
        let mut left = self.factor()?;
        loop {
            if self.take('*') {
                left *= self.factor()?;
            } else if self.take('/') {
                left /= self.factor()?;
            } else {
                return Some(left);
            }
            self.computed = true;
        }
    }

    fn factor(&mut self) -> Option<f64> {
        if self.take('-') {
            return self.factor().map(|v| -v);
        }
        if self.take('(') {
            self.computed = true;
            let value = self.expression()?;
            return self.take(')').then_some(value);
        }
        self.number()
    }

    /// Digits with at most one point, in either order: "1.5", ".5" and "5."
    /// are all numbers people type.
    fn number(&mut self) -> Option<f64> {
        self.peek();
        let start = self.at;
        let mut seen_point = false;
        let mut seen_digit = false;
        while let Some(c) = self.chars.get(self.at).copied() {
            match c {
                '0'..='9' => seen_digit = true,
                '.' if !seen_point => seen_point = true,
                _ => break,
            }
            self.at += 1;
        }
        if !seen_digit {
            return None;
        }
        let text: String = self.chars[start..self.at].iter().collect();
        text.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_operations_work() {
        assert_eq!(calculate("2+2").as_deref(), Some("4"));
        assert_eq!(calculate("10 - 3").as_deref(), Some("7"));
        assert_eq!(calculate("6*7").as_deref(), Some("42"));
        assert_eq!(calculate("9/4").as_deref(), Some("2.25"));
    }

    #[test]
    fn multiplication_binds_tighter_than_addition() {
        assert_eq!(calculate("2+3*4").as_deref(), Some("14"));
        assert_eq!(calculate("(2+3)*4").as_deref(), Some("20"));
        assert_eq!(calculate("10-4/2").as_deref(), Some("8"));
    }

    #[test]
    fn subtraction_and_division_associate_left() {
        assert_eq!(calculate("10-3-2").as_deref(), Some("5"));
        assert_eq!(calculate("64/4/2").as_deref(), Some("8"));
    }

    #[test]
    fn unary_minus_and_decimals_are_understood() {
        assert_eq!(calculate("-3*2").as_deref(), Some("-6"));
        assert_eq!(calculate("2*-3").as_deref(), Some("-6"));
        assert_eq!(calculate("-(1+2)").as_deref(), Some("-3"));
        assert_eq!(calculate("1.5+.5").as_deref(), Some("2"));
        assert_eq!(calculate("0.1+0.2").as_deref(), Some("0.3"));
    }

    #[test]
    fn trailing_zeros_are_trimmed_but_meaningful_digits_kept() {
        assert_eq!(calculate("1/3").as_deref(), Some("0.3333333333"));
        assert_eq!(calculate("2.50*2").as_deref(), Some("5"));
        assert_eq!(calculate("0*-1").as_deref(), Some("0"));
    }

    #[test]
    fn division_by_zero_is_no_result() {
        assert_eq!(calculate("1/0"), None);
        assert_eq!(calculate("0/0"), None);
        assert_eq!(calculate("1/(2-2)"), None);
    }

    #[test]
    fn a_bare_number_is_not_a_calculation() {
        // "7" is the start of something being typed, not a question.
        assert_eq!(calculate("7"), None);
        assert_eq!(calculate("-7"), None);
        assert_eq!(calculate("3.14"), None);
    }

    #[test]
    fn words_typos_and_half_expressions_are_not_calculations() {
        assert_eq!(calculate("firefox"), None);
        assert_eq!(calculate("2+"), None);
        assert_eq!(calculate("(2+3"), None);
        assert_eq!(calculate("2+3)"), None);
        assert_eq!(calculate("2 2"), None);
        assert_eq!(calculate("1..2+1"), None);
        assert_eq!(calculate(""), None);
        assert_eq!(calculate("   "), None);
        assert_eq!(calculate("1password"), None);
    }
}
