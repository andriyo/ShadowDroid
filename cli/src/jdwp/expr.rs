//! Breakpoint condition and log-expression grammar (design §5.3): the
//! deterministic path grammar (`this`, locals, `$exception`, `.field`,
//! `[index]`) plus comparisons, literals, `&&`, `||`, `!`, and parentheses.
//! No method calls, assignments, or arithmetic.
//!
//! ```text
//! expr    := or
//! or      := and ("||" and)*
//! and     := unary ("&&" unary)*
//! unary   := "!" unary | compare
//! compare := primary (("==" | "!=" | "<" | "<=" | ">" | ">=") primary)?
//! primary := "(" expr ")" | number | string | "true" | "false" | "null" | path
//! path    := root ("." name | "[" integer "]")*
//! ```
//!
//! Paths are kept as source text and handed to the session's path reader,
//! so the condition grammar and `debug eval` always agree on what a path is.

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Literal(Literal),
    Path(String),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Compare(Box<Expr>, CompareOp, Box<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CompareOp {
    fn symbol(self) -> &'static str {
        match self {
            CompareOp::Eq => "==",
            CompareOp::Ne => "!=",
            CompareOp::Lt => "<",
            CompareOp::Le => "<=",
            CompareOp::Gt => ">",
            CompareOp::Ge => ">=",
        }
    }
}

/// A value an expression evaluates to. Object references other than strings
/// and boxed primitives stay opaque: they compare by identity or to `null`.
#[derive(Clone, Debug, PartialEq)]
pub enum EvalValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Char(char),
    Str(String),
    /// A non-null object that is not a string; `text` is its rendering.
    Object {
        id: u64,
        text: String,
    },
}

impl EvalValue {
    /// Truthiness (design §5.3): `true`, a non-null reference, a non-zero
    /// number, or a non-empty string.
    pub fn truthy(&self) -> bool {
        match self {
            EvalValue::Null => false,
            EvalValue::Bool(value) => *value,
            EvalValue::Int(value) => *value != 0,
            EvalValue::Float(value) => *value != 0.0,
            EvalValue::Char(value) => *value != '\0',
            EvalValue::Str(value) => !value.is_empty(),
            EvalValue::Object { .. } => true,
        }
    }

    /// Text for a log message.
    pub fn display(&self) -> String {
        match self {
            EvalValue::Null => "null".into(),
            EvalValue::Bool(value) => value.to_string(),
            EvalValue::Int(value) => value.to_string(),
            EvalValue::Float(value) => value.to_string(),
            EvalValue::Char(value) => value.to_string(),
            EvalValue::Str(value) => value.clone(),
            EvalValue::Object { text, .. } => text.clone(),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            EvalValue::Null => "null",
            EvalValue::Bool(_) => "boolean",
            EvalValue::Int(_) | EvalValue::Float(_) => "number",
            EvalValue::Char(_) => "char",
            EvalValue::Str(_) => "string",
            EvalValue::Object { .. } => "object",
        }
    }
}

impl From<&Literal> for EvalValue {
    fn from(literal: &Literal) -> Self {
        match literal {
            Literal::Null => EvalValue::Null,
            Literal::Bool(value) => EvalValue::Bool(*value),
            Literal::Int(value) => EvalValue::Int(*value),
            Literal::Float(value) => EvalValue::Float(*value),
            Literal::Str(value) => EvalValue::Str(value.clone()),
        }
    }
}

/// Apply a comparison. A type mismatch is an evaluation error, never a
/// silent `false`.
pub fn compare(left: &EvalValue, op: CompareOp, right: &EvalValue) -> Result<bool, String> {
    use EvalValue::*;
    let ordering = match (left, right) {
        (Null, Null) => Some(std::cmp::Ordering::Equal),
        (Null, Object { .. } | Str(_)) | (Object { .. } | Str(_), Null) => {
            return equality_only(op, false);
        }
        (Object { id: a, .. }, Object { id: b, .. }) => return equality_only(op, a == b),
        (Bool(a), Bool(b)) => return equality_only(op, a == b),
        (Str(a), Str(b)) => Some(a.cmp(b)),
        (Char(a), Char(b)) => Some(a.cmp(b)),
        (Char(a), Str(b)) | (Str(b), Char(a)) if b.chars().count() == 1 => {
            let b = b.chars().next().expect("one char");
            let ordering = a.cmp(&b);
            Some(if matches!(left, Str(_)) {
                ordering.reverse()
            } else {
                ordering
            })
        }
        (Int(a), Int(b)) => Some(a.cmp(b)),
        (Char(a), Int(b)) => Some(i64::from(u32::from(*a)).cmp(b)),
        (Int(a), Char(b)) => Some(a.cmp(&i64::from(u32::from(*b)))),
        (Int(_) | Float(_), Int(_) | Float(_)) => {
            let (a, b) = (as_f64(left), as_f64(right));
            a.partial_cmp(&b)
        }
        _ => {
            return Err(format!(
                "cannot compare {} {} {}",
                left.kind(),
                op.symbol(),
                right.kind()
            ));
        }
    };
    let Some(ordering) = ordering else {
        // NaN: only `!=` holds.
        return Ok(op == CompareOp::Ne);
    };
    use std::cmp::Ordering::*;
    Ok(match op {
        CompareOp::Eq => ordering == Equal,
        CompareOp::Ne => ordering != Equal,
        CompareOp::Lt => ordering == Less,
        CompareOp::Le => ordering != Greater,
        CompareOp::Gt => ordering == Greater,
        CompareOp::Ge => ordering != Less,
    })
}

fn equality_only(op: CompareOp, equal: bool) -> Result<bool, String> {
    match op {
        CompareOp::Eq => Ok(equal),
        CompareOp::Ne => Ok(!equal),
        other => Err(format!(
            "`{}` needs numbers, strings, or chars",
            other.symbol()
        )),
    }
}

fn as_f64(value: &EvalValue) -> f64 {
    match value {
        EvalValue::Int(v) => *v as f64,
        EvalValue::Float(v) => *v,
        _ => f64::NAN,
    }
}

/// Parse an expression; the error names the position.
pub fn parse(source: &str) -> Result<Expr, String> {
    let mut parser = Parser {
        src: source,
        pos: 0,
    };
    parser.skip_ws();
    if parser.at_end() {
        return Err("empty expression".into());
    }
    let expr = parser.or()?;
    parser.skip_ws();
    if !parser.at_end() {
        return Err(format!(
            "unexpected `{}` at offset {}",
            &source[parser.pos..],
            parser.pos
        ));
    }
    Ok(expr)
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn rest(&self) -> &str {
        &self.src[self.pos..]
    }

    fn at_end(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn skip_ws(&mut self) {
        let trimmed = self.rest().trim_start();
        self.pos = self.src.len() - trimmed.len();
    }

    fn eat(&mut self, token: &str) -> bool {
        self.skip_ws();
        if self.rest().starts_with(token) {
            self.pos += token.len();
            true
        } else {
            false
        }
    }

    fn or(&mut self) -> Result<Expr, String> {
        let mut left = self.and()?;
        while self.eat("||") {
            let right = self.and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, String> {
        let mut left = self.unary()?;
        while self.eat("&&") {
            let right = self.unary()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        self.skip_ws();
        if self.rest().starts_with('!') && !self.rest().starts_with("!=") {
            self.pos += 1;
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        self.compare()
    }

    fn compare(&mut self) -> Result<Expr, String> {
        let left = self.primary()?;
        self.skip_ws();
        // Two-character operators first.
        let op = [
            ("==", CompareOp::Eq),
            ("!=", CompareOp::Ne),
            ("<=", CompareOp::Le),
            (">=", CompareOp::Ge),
            ("<", CompareOp::Lt),
            (">", CompareOp::Gt),
        ]
        .into_iter()
        .find(|(token, _)| self.rest().starts_with(token));
        let Some((token, op)) = op else {
            return Ok(left);
        };
        self.pos += token.len();
        let right = self.primary()?;
        Ok(Expr::Compare(Box::new(left), op, Box::new(right)))
    }

    fn primary(&mut self) -> Result<Expr, String> {
        self.skip_ws();
        let Some(first) = self.rest().chars().next() else {
            return Err("expression ended early".into());
        };
        if first == '(' {
            self.pos += 1;
            let inner = self.or()?;
            if !self.eat(")") {
                return Err(format!("missing `)` at offset {}", self.pos));
            }
            return Ok(inner);
        }
        if first == '"' {
            return self.string().map(|s| Expr::Literal(Literal::Str(s)));
        }
        if first.is_ascii_digit()
            || (first == '-' && self.rest()[1..].starts_with(|c: char| c.is_ascii_digit()))
        {
            return self.number();
        }
        if first.is_alphabetic() || first == '_' || first == '$' {
            return self.path_or_keyword();
        }
        Err(format!("unexpected `{first}` at offset {}", self.pos))
    }

    fn string(&mut self) -> Result<String, String> {
        let start = self.pos;
        self.pos += 1;
        let mut out = String::new();
        let mut chars = self.rest().char_indices();
        while let Some((offset, ch)) = chars.next() {
            match ch {
                '"' => {
                    self.pos += offset + 1;
                    return Ok(out);
                }
                '\\' => match chars.next() {
                    Some((_, 'n')) => out.push('\n'),
                    Some((_, 't')) => out.push('\t'),
                    Some((_, other)) => out.push(other),
                    None => break,
                },
                other => out.push(other),
            }
        }
        Err(format!("unterminated string starting at offset {start}"))
    }

    fn number(&mut self) -> Result<Expr, String> {
        let text: String = self
            .rest()
            .char_indices()
            .take_while(|(i, c)| {
                c.is_ascii_digit() || *c == '.' || (*i == 0 && *c == '-') || *c == '_'
            })
            .map(|(_, c)| c)
            .collect();
        self.pos += text.len();
        // Kotlin/Java literal suffixes are accepted and ignored.
        if self.rest().starts_with(['L', 'l', 'f', 'F', 'd', 'D']) {
            self.pos += 1;
        }
        let clean = text.replace('_', "");
        if clean.contains('.') {
            clean
                .parse()
                .map(|v| Expr::Literal(Literal::Float(v)))
                .map_err(|_| format!("bad number `{text}`"))
        } else {
            clean
                .parse()
                .map(|v| Expr::Literal(Literal::Int(v)))
                .map_err(|_| format!("bad number `{text}`"))
        }
    }

    fn path_or_keyword(&mut self) -> Result<Expr, String> {
        let start = self.pos;
        let ident_len = self
            .rest()
            .char_indices()
            .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '$'))
            .map_or(self.rest().len(), |(i, _)| i);
        let ident = &self.rest()[..ident_len];
        let keyword = match ident {
            "true" => Some(Literal::Bool(true)),
            "false" => Some(Literal::Bool(false)),
            "null" => Some(Literal::Null),
            _ => None,
        };
        self.pos += ident_len;
        if let Some(literal) = keyword {
            return Ok(Expr::Literal(literal));
        }
        loop {
            if self.rest().starts_with('.') {
                self.pos += 1;
                let len = self
                    .rest()
                    .char_indices()
                    .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '$'))
                    .map_or(self.rest().len(), |(i, _)| i);
                if len == 0 {
                    return Err(format!("empty field name at offset {}", self.pos));
                }
                if self.rest()[len..].trim_start().starts_with('(') {
                    return Err("method calls are not allowed in conditions".into());
                }
                self.pos += len;
            } else if self.rest().starts_with('[') {
                let Some(close) = self.rest().find(']') else {
                    return Err(format!("missing `]` at offset {}", self.pos));
                };
                let index = self.rest()[1..close].trim();
                if index.parse::<i64>().is_err() {
                    return Err(format!("array index must be an integer, got `{index}`"));
                }
                self.pos += close + 1;
            } else if self.rest().trim_start().starts_with('(') {
                return Err("method calls are not allowed in conditions".into());
            } else {
                break;
            }
        }
        Ok(Expr::Path(self.src[start..self.pos].to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(p: &str) -> Box<Expr> {
        Box::new(Expr::Path(p.into()))
    }

    #[test]
    fn parses_paths_literals_and_precedence() {
        assert_eq!(
            parse("this.counter > 3 && tag != null || !done").unwrap(),
            Expr::Or(
                Box::new(Expr::And(
                    Box::new(Expr::Compare(
                        path("this.counter"),
                        CompareOp::Gt,
                        Box::new(Expr::Literal(Literal::Int(3)))
                    )),
                    Box::new(Expr::Compare(
                        path("tag"),
                        CompareOp::Ne,
                        Box::new(Expr::Literal(Literal::Null))
                    )),
                )),
                Box::new(Expr::Not(path("done"))),
            )
        );
        assert_eq!(
            parse(r#"(items[0].name == "a\"b")"#).unwrap(),
            Expr::Compare(
                path("items[0].name"),
                CompareOp::Eq,
                Box::new(Expr::Literal(Literal::Str("a\"b".into())))
            )
        );
        assert_eq!(
            parse("$exception").unwrap(),
            Expr::Path("$exception".into())
        );
        assert_eq!(
            parse("x <= -2.5f").unwrap(),
            Expr::Compare(
                path("x"),
                CompareOp::Le,
                Box::new(Expr::Literal(Literal::Float(-2.5)))
            )
        );
        assert_eq!(
            parse("n >= 1_000L").unwrap(),
            Expr::Compare(
                path("n"),
                CompareOp::Ge,
                Box::new(Expr::Literal(Literal::Int(1000)))
            )
        );
    }

    #[test]
    fn rejects_calls_and_malformed_input() {
        for bad in [
            "",
            "foo()",
            "this.toString()",
            "a ==",
            "(a",
            "a[x]",
            "\"open",
            "a + b",
            "a.",
        ] {
            assert!(parse(bad).is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn comparisons_follow_types() {
        use EvalValue::*;
        assert_eq!(compare(&Int(3), CompareOp::Lt, &Float(3.5)), Ok(true));
        assert_eq!(
            compare(&Str("b".into()), CompareOp::Gt, &Str("a".into())),
            Ok(true)
        );
        assert_eq!(compare(&Null, CompareOp::Eq, &Null), Ok(true));
        let object = Object {
            id: 5,
            text: "x".into(),
        };
        assert_eq!(compare(&object, CompareOp::Ne, &Null), Ok(true));
        assert_eq!(compare(&object, CompareOp::Eq, &object.clone()), Ok(true));
        assert_eq!(
            compare(&Char('a'), CompareOp::Eq, &Str("a".into())),
            Ok(true)
        );
        assert_eq!(
            compare(&Str("b".into()), CompareOp::Gt, &Char('a')),
            Ok(true)
        );
        assert_eq!(compare(&Char('a'), CompareOp::Eq, &Int(97)), Ok(true));
        assert_eq!(compare(&Float(f64::NAN), CompareOp::Ne, &Int(1)), Ok(true));
        assert!(compare(&Bool(true), CompareOp::Lt, &Bool(false)).is_err());
        assert!(compare(&Int(1), CompareOp::Eq, &Str("1".into())).is_err());
        assert!(compare(&object, CompareOp::Gt, &Null).is_err());
    }

    #[test]
    fn truthiness_matches_the_design() {
        use EvalValue::*;
        assert!(Bool(true).truthy());
        assert!(!Int(0).truthy());
        assert!(Float(0.5).truthy());
        assert!(!Str(String::new()).truthy());
        assert!(!Null.truthy());
        assert!(
            Object {
                id: 1,
                text: String::new()
            }
            .truthy()
        );
        assert_eq!(Str("x".into()).display(), "x");
        assert_eq!(Null.display(), "null");
    }
}
