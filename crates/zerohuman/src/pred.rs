//! The predicate language shared by wants, postconditions and requirements.
//!
//! ```text
//! invoice(customer=customer(name='Acme')).exists
//! invoice(id=$id).status='sent'
//! report(invoices=[invoice(customer=customer(name='Acme')), $A]).exists
//! ```
//! A predicate names an entity, identifies it by arguments, and states one field.
//! `$name` is a variable bound by unification, `$name[]` spreads a list,
//! `$A.id` refers to the output of node A, and a nested `entity(...)` is a
//! reference the compiler resolves to a node.

use std::fmt;

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Error)]
pub enum ParseError {
    #[error("expected {what} at byte {at}")]
    Expected { what: String, at: usize },
    #[error("unterminated string")]
    Unterminated,
    #[error("bad number '{0}'")]
    BadNumber(String),
    #[error("unexpected {found:?} at byte {at}")]
    Unexpected { found: Option<char>, at: usize },
    #[error("trailing input at byte {at}: {rest}")]
    Trailing { at: usize, rest: String },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Str(String),
    Num(i64),
    Bool(bool),
    List(Vec<Val>),
    /// `$name` or `$name[]`
    Var(String, bool),
    /// `$node.field`
    Ref(String, String),
    /// `entity(args)` used as a value: something to resolve to an id.
    Entity(Box<Pred>),
    /// `each([a,b,c])`: the want is unrolled once per element before compiling.
    Each(Vec<Val>),
    /// `all(T)`: one want, in which `T`'s `each(...)` becomes a list of every element.
    /// `each` fans a want out; `all` collects into one.
    All(Box<Val>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pred {
    pub entity: String,
    pub args: Vec<(String, Val)>,
    /// Empty for a bare entity reference.
    pub field: String,
    pub value: Val,
}

impl Pred {
    pub fn parse(src: &str) -> Result<Pred, ParseError> {
        let mut p = Parser { s: src.as_bytes(), i: 0 };
        let pred = p.pred()?;
        p.ws();
        if p.i != p.s.len() {
            return Err(ParseError::Trailing { at: p.i, rest: src[p.i..].to_string() });
        }
        Ok(pred)
    }

    pub fn arg(&self, k: &str) -> Option<&Val> {
        self.args.iter().find(|(n, _)| n == k).map(|(_, v)| v)
    }

    pub fn is_existence(&self) -> bool {
        self.field == "exists" || self.field == "resolved"
    }

    /// The same entity and identification, but asking only that it exists.
    pub fn as_exists(&self) -> Pred {
        Pred { entity: self.entity.clone(), args: self.args.clone(), field: "exists".into(), value: Val::Bool(true) }
    }

    pub fn pick(&self, i: usize) -> Pred {
        Pred {
            entity: self.entity.clone(),
            args: self.args.iter().map(|(k, v)| (k.clone(), v.pick(i))).collect(),
            field: self.field.clone(),
            value: self.value.pick(i),
        }
    }

    pub fn each_len(&self) -> Option<usize> {
        self.args.iter().find_map(|(_, v)| v.each_len()).or_else(|| self.value.each_len())
    }

    /// Expand the two quantifiers:
    ///
    /// - `all(T)` collects: `report(invoices=[all(invoice(customer=each([A,B])))])` is one report
    ///   over two invoices;
    /// - `each(...)` fans out: `invoice(customer=each([A,B]))` is two wants.
    ///
    /// `all` is resolved first, so an `each` inside it never multiplies the want.
    pub fn unroll(&self) -> Vec<Pred> {
        let collected = Pred {
            entity: self.entity.clone(),
            args: self.args.iter().map(|(k, v)| (k.clone(), collect_all(v))).collect(),
            field: self.field.clone(),
            value: collect_all(&self.value),
        };
        match collected.each_len() {
            Some(n) => (0..n).map(|i| collected.pick(i)).collect(),
            None => vec![collected],
        }
    }

    /// Substitute bound variables. Unbound variables stay as they are.
    pub fn subst(&self, bind: &dyn Fn(&str) -> Option<Val>) -> Pred {
        Pred {
            entity: self.entity.clone(),
            args: self.args.iter().map(|(k, v)| (k.clone(), v.subst(bind))).collect(),
            field: self.field.clone(),
            value: self.value.subst(bind),
        }
    }
}

/// Turn every `all(T)` into the list of `T`'s expansions. Recursive, innermost first.
fn collect_all(v: &Val) -> Val {
    match v {
        Val::All(inner) => {
            let inner = collect_all(inner);
            match inner.each_len() {
                Some(n) => Val::List((0..n).map(|i| inner.pick(i)).collect()),
                // `all` of something with nothing to expand is that one thing, in a list.
                None => Val::List(vec![inner]),
            }
        }
        Val::List(xs) => {
            // A list whose element is an `all` splices that list in, so
            // `[all(x)]` and `all(x)` mean the same thing in an argument.
            let mut out = Vec::new();
            for x in xs {
                match collect_all(x) {
                    Val::List(inner) if matches!(x, Val::All(_)) => out.extend(inner),
                    other => out.push(other),
                }
            }
            Val::List(out)
        }
        Val::Each(xs) => Val::Each(xs.iter().map(collect_all).collect()),
        Val::Entity(p) => Val::Entity(Box::new(Pred {
            entity: p.entity.clone(),
            args: p.args.iter().map(|(k, a)| (k.clone(), collect_all(a))).collect(),
            field: p.field.clone(),
            value: collect_all(&p.value),
        })),
        other => other.clone(),
    }
}

impl Val {
    pub fn subst(&self, bind: &dyn Fn(&str) -> Option<Val>) -> Val {
        match self {
            // TODO(each): a spread variable should splice a bound list into its parent list.
            Val::Var(n, _spread) => bind(n).unwrap_or_else(|| self.clone()),
            Val::List(xs) => Val::List(xs.iter().map(|x| x.subst(bind)).collect()),
            Val::Entity(p) => Val::Entity(Box::new(p.subst(bind))),
            Val::Each(xs) => Val::Each(xs.iter().map(|x| x.subst(bind)).collect()),
            Val::All(x) => Val::All(Box::new(x.subst(bind))),
            other => other.clone(),
        }
    }

    /// Replace every `each(...)` with its i-th element.
    pub fn pick(&self, i: usize) -> Val {
        match self {
            Val::Each(xs) => xs.get(i).cloned().unwrap_or(Val::Bool(false)),
            // `all` is resolved before picking, so nothing inside it fans out.
            Val::All(x) => Val::All(x.clone()),
            Val::List(xs) => Val::List(xs.iter().map(|x| x.pick(i)).collect()),
            Val::Entity(p) => Val::Entity(Box::new(p.pick(i))),
            other => other.clone(),
        }
    }

    /// The length of the first `each(...)` found, if any.
    pub fn each_len(&self) -> Option<usize> {
        match self {
            Val::Each(xs) => Some(xs.len()),
            Val::All(_) => None,
            Val::List(xs) => xs.iter().find_map(|x| x.each_len()),
            Val::Entity(p) => p.each_len(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        if let Val::Str(s) = self {
            Some(s)
        } else {
            None
        }
    }
}

impl fmt::Display for Val {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Val::Str(s) => write!(f, "'{}'", s.replace('\'', "\\'")),
            Val::Num(n) => write!(f, "{n}"),
            Val::Bool(b) => write!(f, "{b}"),
            Val::List(xs) => {
                write!(f, "[")?;
                for (i, x) in xs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, "{x}")?;
                }
                write!(f, "]")
            }
            Val::Var(n, spread) => write!(f, "${n}{}", if *spread { "[]" } else { "" }),
            Val::All(x) => write!(f, "all({x})"),
            Val::Each(xs) => {
                write!(f, "each([")?;
                for (i, x) in xs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, "{x}")?;
                }
                write!(f, "])")
            }
            Val::Ref(n, field) => write!(f, "${n}.{field}"),
            Val::Entity(p) => write!(f, "{p}"),
        }
    }
}

impl fmt::Display for Pred {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}(", self.entity)?;
        for (i, (k, v)) in self.args.iter().enumerate() {
            if i > 0 {
                write!(f, ",")?;
            }
            write!(f, "{k}={v}")?;
        }
        write!(f, ")")?;
        if !self.field.is_empty() {
            write!(f, ".{}", self.field)?;
            if !(self.is_existence() && self.value == Val::Bool(true)) {
                write!(f, "={}", self.value)?;
            }
        }
        Ok(())
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.i < self.s.len() && (self.s[self.i] as char).is_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), ParseError> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(ParseError::Expected { what: format!("'{}'", c as char), at: self.i })
        }
    }

    fn ident(&mut self) -> Result<String, ParseError> {
        self.ws();
        let start = self.i;
        while self.i < self.s.len() && ((self.s[self.i] as char).is_alphanumeric() || self.s[self.i] == b'_') {
            self.i += 1;
        }
        if start == self.i {
            return Err(ParseError::Expected { what: "identifier".into(), at: self.i });
        }
        Ok(std::str::from_utf8(&self.s[start..self.i]).unwrap().to_string())
    }

    fn pred(&mut self) -> Result<Pred, ParseError> {
        let entity = self.ident()?;
        self.expect(b'(')?;
        let mut args = Vec::new();
        if !self.eat(b')') {
            loop {
                let k = self.ident()?;
                self.expect(b'=')?;
                let v = self.value()?;
                args.push((k, v));
                if self.eat(b')') {
                    break;
                }
                self.expect(b',')?;
            }
        }
        let mut field = String::new();
        let mut value = Val::Bool(true);
        if self.eat(b'.') {
            field = self.ident()?;
            if self.eat(b'=') {
                value = self.value()?;
            }
        }
        Ok(Pred { entity, args, field, value })
    }

    fn value(&mut self) -> Result<Val, ParseError> {
        self.ws();
        match self.peek() {
            Some(b'\'') => {
                self.i += 1;
                let mut out = String::new();
                loop {
                    match self.peek() {
                        None => return Err(ParseError::Unterminated),
                        Some(b'\\') => {
                            self.i += 1;
                            if let Some(c) = self.peek() {
                                out.push(c as char);
                                self.i += 1;
                            }
                        }
                        Some(b'\'') => {
                            self.i += 1;
                            break;
                        }
                        Some(c) => {
                            out.push(c as char);
                            self.i += 1;
                        }
                    }
                }
                Ok(Val::Str(out))
            }
            Some(b'$') => {
                self.i += 1;
                let name = self.ident()?;
                if self.peek() == Some(b'[') && self.s.get(self.i + 1) == Some(&b']') {
                    self.i += 2;
                    return Ok(Val::Var(name, true));
                }
                if self.peek() == Some(b'.') {
                    self.i += 1;
                    let field = self.ident()?;
                    return Ok(Val::Ref(name, field));
                }
                Ok(Val::Var(name, false))
            }
            Some(b'[') => {
                self.i += 1;
                let mut xs = Vec::new();
                if !self.eat(b']') {
                    loop {
                        xs.push(self.value()?);
                        if self.eat(b']') {
                            break;
                        }
                        self.expect(b',')?;
                    }
                }
                Ok(Val::List(xs))
            }
            Some(c) if c.is_ascii_digit() || c == b'-' => {
                let start = self.i;
                self.i += 1;
                while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                    self.i += 1;
                }
                let txt = std::str::from_utf8(&self.s[start..self.i]).unwrap();
                txt.parse::<i64>().map(Val::Num).map_err(|_| ParseError::BadNumber(txt.to_string()))
            }
            Some(c) if (c as char).is_alphabetic() => {
                let save = self.i;
                let word = self.ident()?;
                match word.as_str() {
                    "true" => Ok(Val::Bool(true)),
                    "false" => Ok(Val::Bool(false)),
                    "all" => {
                        self.expect(b'(')?;
                        let inner = self.value()?;
                        self.expect(b')')?;
                        Ok(Val::All(Box::new(inner)))
                    }
                    "each" => {
                        self.expect(b'(')?;
                        let inner = self.value()?;
                        self.expect(b')')?;
                        match inner {
                            Val::List(xs) => Ok(Val::Each(xs)),
                            other => Ok(Val::Each(vec![other])),
                        }
                    }
                    _ => {
                        self.i = save;
                        let p = self.pred()?;
                        Ok(Val::Entity(Box::new(p)))
                    }
                }
            }
            other => Err(ParseError::Unexpected { found: other.map(|c| c as char), at: self.i }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for src in [
            "invoice(customer=customer(name='Acme')).exists",
            "invoice(id=$id).status='sent'",
            "report(invoices=[$A.id,$B.id]).exists",
            "customer(name=$name).resolved",
            "invoice(id=$invoice_ids[]).exists",
            "invoice(id=$id).approved=true",
        ] {
            let p = Pred::parse(src).unwrap();
            assert_eq!(p.to_string(), src, "round trip");
        }
    }

    #[test]
    fn each_unrolls_one_predicate_per_element() {
        let p = Pred::parse("invoice(customer=customer(name=each(['Acme','Globex','Initech']))).status='sent'").unwrap();
        assert_eq!(p.to_string(), "invoice(customer=customer(name=each(['Acme','Globex','Initech']))).status='sent'");
        let rolled = p.unroll();
        assert_eq!(rolled.len(), 3);
        assert_eq!(rolled[1].to_string(), "invoice(customer=customer(name='Globex')).status='sent'");
        assert_eq!(Pred::parse("invoice(id=3).exists").unwrap().unroll().len(), 1);
    }

    #[test]
    fn each_fans_out_and_all_collects() {
        // all(...) collects: one report over three invoices.
        let p = Pred::parse("report(invoices=[all(invoice(customer=each(['Acme','Globex','Initech'])))]).exists").unwrap();
        let rolled = p.unroll();
        assert_eq!(rolled.len(), 1);
        assert_eq!(rolled[0].to_string(), "report(invoices=[invoice(customer='Acme'),invoice(customer='Globex'),invoice(customer='Initech')]).exists");
        // each(...) fans out: one report per invoice.
        let p = Pred::parse("report(invoices=[invoice(customer=each(['Acme','Globex']))]).exists").unwrap();
        let rolled = p.unroll();
        assert_eq!(rolled.len(), 2);
        assert_eq!(rolled[0].to_string(), "report(invoices=[invoice(customer='Acme')]).exists");
        // and in a plain argument.
        assert_eq!(Pred::parse("invoice(customer=each(['Acme','Globex'])).status='sent'").unwrap().unroll().len(), 2);
        // A list with neither is untouched.
        let p = Pred::parse("report(invoices=[$A.id,$B.id]).exists").unwrap();
        assert_eq!(p.unroll()[0].to_string(), "report(invoices=[$A.id,$B.id]).exists");
        // all(...) round-trips through Display.
        let src = "report(invoices=[all(invoice(customer=each(['A'])))]).exists";
        assert_eq!(Pred::parse(src).unwrap().to_string(), src);
    }

    #[test]
    fn errors_are_typed() {
        assert!(matches!(Pred::parse("invoice(").unwrap_err(), ParseError::Expected { .. }));
        assert_eq!(Pred::parse("invoice(name='x").unwrap_err(), ParseError::Unterminated);
        assert!(matches!(Pred::parse("invoice() extra").unwrap_err(), ParseError::Trailing { .. }));
    }

    #[test]
    fn nested_entity_is_a_value() {
        let p = Pred::parse("invoice(customer=customer(name='Acme')).exists").unwrap();
        match p.arg("customer") {
            Some(Val::Entity(inner)) => assert_eq!(inner.entity, "customer"),
            other => panic!("{other:?}"),
        }
        assert!(p.is_existence());
    }
}
