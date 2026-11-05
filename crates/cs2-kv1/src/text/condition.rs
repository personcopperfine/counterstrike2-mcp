//! Port of upstream `KVConditionEvaluator`: the C-style infix evaluator behind `[$COND]`.
//!
//! `expression := term { ('&&' | '||') term }`, `term := '!' term | '(' expression ')' |
//! `$<identifier>` | decimal digits`. `&&` and `||` share one precedence level and are
//! left-associative, matching Valve's tier1 `CExpressionEvaluator` (upstream's own note).
//! `$name` is true when `name` is one of the defined symbols, compared case-insensitively.
//! A bare digit run is a constant: `0` is false, any digit other than `0` makes it true (`10` is true, `00` is false), while
//! `$0` is a variable lookup (Valve only treats bare digits as constants).
//!
//! # Differences from upstream
//!
//! * **The failure is a value, not an exception.** Every `InvalidOperationException` upstream
//!   throws becomes an [`EvalError`] carrying the same message, which the reader wraps in an
//!   [`crate::Error::Syntax`] naming the conditional's own position, as upstream does with
//!   `Invalid conditional syntax "…"`.
//! * **Symbol matching is `eq_ignore_ascii_case`** (upstream uses the runtime's
//!   `OrdinalIgnoreCase` comparer). For the ASCII symbol names both Valve and this crate deal
//!   with, the two agree; they could differ for a symbol with a non-ASCII letter.
//! * **A `$` name is ASCII letters, digits and `_`.** .NET's `char.IsLetterOrDigit` (categories
//!   L* and Nd) takes every Unicode letter and decimal digit; `std` cannot ask a `char` for its
//!   general category. `char::is_alphanumeric` is no substitute: it also accepts combining marks
//!   (U+093E, a Devanagari vowel sign), superscript digits and Roman numerals that upstream
//!   rejects. Matching upstream for the common case (a mark must end the name) is worth more
//!   than accepting non-ASCII letters, so a non-ASCII character ends the name and then fails as
//!   `Unexpected …`. Only a hand-written conditional naming a non-ASCII symbol can tell the
//!   difference.
//! * Nothing else: the depth cap (128, so thousands of `(` or `!` cannot overflow the stack),
//!   the accept/reject set for ASCII input and the evaluation result are upstream's.

/// Bail out before pathologically nested input (thousands of `(` or `!`) can overflow the
/// stack. Same constant and same `>` check as upstream.
pub(crate) const MAXIMUM_NESTING_DEPTH: usize = 128;

/// Why a conditional could not be evaluated. The message is the upstream
/// `InvalidOperationException` message; the reader adds the position.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EvalError(pub String);

type EvalResult = Result<bool, EvalError>;

/// Evaluates `text` against `defined`. Symbols are given without the leading `$` (`WIN32`),
/// the way [`crate::text::ReadOptions::conditions`] spells them.
pub(crate) fn evaluate_condition(defined: &[String], text: &str) -> EvalResult {
    let chars: Vec<char> = text.chars().collect();
    ConditionEvaluator { defined, chars: &chars, position: 0 }.evaluate()
}

struct ConditionEvaluator<'a> {
    defined: &'a [String],
    /// Random access is needed across the expression, which a `&str` does not offer per
    /// position; the characters are collected once per evaluation.
    chars: &'a [char],
    position: usize,
}

impl ConditionEvaluator<'_> {
    fn evaluate(&mut self) -> EvalResult {
        self.position = 0;
        let result = self.evaluate_expression(0)?;

        self.skip_whitespace();
        if let Some(&c) = self.peek() {
            return Err(EvalError(format!("Unexpected '{c}' after end of expression.")));
        }

        Ok(result)
    }

    fn peek(&self) -> Option<&char> {
        self.chars.get(self.position)
    }

    /// `expression := term { ('&&' | '||') term }`
    fn evaluate_expression(&mut self, depth: usize) -> EvalResult {
        let mut result = self.evaluate_term(depth)?;

        loop {
            self.skip_whitespace();

            if self.try_skip_operator('&') {
                result &= self.evaluate_term(depth)?;
            } else if self.try_skip_operator('|') {
                result |= self.evaluate_term(depth)?;
            } else {
                break;
            }
        }

        Ok(result)
    }

    /// `term := '!' term | '(' expression ')' | '$' identifier | digits`
    fn evaluate_term(&mut self, depth: usize) -> EvalResult {
        if depth > MAXIMUM_NESTING_DEPTH {
            return Err(EvalError("Expression is nested too deeply.".into()));
        }

        self.skip_whitespace();

        let Some(&current) = self.peek() else {
            return Err(EvalError("Unexpected end of expression.".into()));
        };

        match current {
            '!' => {
                self.position += 1;
                Ok(!self.evaluate_term(depth + 1)?)
            }
            '(' => {
                self.position += 1;
                let result = self.evaluate_expression(depth + 1)?;

                self.skip_whitespace();
                if self.peek() != Some(&')') {
                    return Err(EvalError("Unterminated bracketed expression.".into()));
                }

                self.position += 1;
                Ok(result)
            }
            '$' => {
                self.position += 1;
                let name = self.read_variable_name();
                Ok(self.evaluate_variable(&name))
            }
            '0'..='9' => self.read_numeric_literal(),
            other => Err(EvalError(format!("Unexpected '{other}'."))),
        }
    }

    fn evaluate_variable(&self, variable: &str) -> bool {
        self.defined.iter().any(|defined| defined.eq_ignore_ascii_case(variable))
    }

    /// `&&` and `||` are two-character operators; a single one is not an operator at all
    /// (it falls through to the caller, which then reports the stray character).
    fn try_skip_operator(&mut self, operator: char) -> bool {
        if self.position + 1 < self.chars.len()
            && self.chars[self.position] == operator
            && self.chars[self.position + 1] == operator
        {
            self.position += 2;
            return true;
        }
        false
    }

    /// A bare digit run is a numeric literal: `0` is false, any digit that is not `0` makes it
    /// true, so `00` is false and `10` is true.
    fn read_numeric_literal(&mut self) -> EvalResult {
        let mut is_non_zero = false;
        while let Some(&c) = self.peek() {
            if !c.is_ascii_digit() {
                break;
            }
            is_non_zero |= c != '0';
            self.position += 1;
        }
        Ok(is_non_zero)
    }

    fn read_variable_name(&mut self) -> String {
        let start = self.position;
        while self.peek().is_some_and(|&c| c.is_ascii_alphanumeric() || c == '_') {
            self.position += 1;
        }
        self.chars[start..self.position].iter().collect()
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(|c| c.is_whitespace()) {
            self.position += 1;
        }
    }
}
