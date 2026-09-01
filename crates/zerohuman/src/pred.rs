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
    pub fn parse(src: &str) -> Result<Pred, String> {
        let mut p = Parser { s: src.as_bytes(), i: 0 };
        let pred = p.pred()?;
        p.ws();
        if p.i != p.s.len() {
            return Err(format!("trailing input at {}: {}", p.i, &src[p.i..]));
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

impl Val {
    pub fn subst(&self, bind: &dyn Fn(&str) -> Option<Val>) -> Val {
        match self {
            // TODO(each): a spread variable should splice a bound list into its parent list.
            Val::Var(n, _spread) => bind(n).unwrap_or_else(|| self.clone()),
            Val::List(xs) => Val::List(xs.iter().map(|x| x.subst(bind)).collect()),
            Val::Entity(p) => Val::Entity(Box::new(p.subst(bind))),
            other => other.clone(),
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

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(format!("expected '{}' at {}", c as char, self.i))
        }
    }

    fn ident(&mut self) -> Result<String, String> {
        self.ws();
        let start = self.i;
        while self.i < self.s.len() && ((self.s[self.i] as char).is_alphanumeric() || self.s[self.i] == b'_') {
            self.i += 1;
        }
        if start == self.i {
            return Err(format!("expected identifier at {}", self.i));
        }
        Ok(std::str::from_utf8(&self.s[start..self.i]).unwrap().to_string())
    }

    fn pred(&mut self) -> Result<Pred, String> {
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

    fn value(&mut self) -> Result<Val, String> {
        self.ws();
        match self.peek() {
            Some(b'\'') => {
                self.i += 1;
                let mut out = String::new();
                loop {
                    match self.peek() {
                        None => return Err("unterminated string".into()),
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
                txt.parse::<i64>().map(Val::Num).map_err(|e| e.to_string())
            }
            Some(c) if (c as char).is_alphabetic() => {
                let save = self.i;
                let word = self.ident()?;
                match word.as_str() {
                    "true" => Ok(Val::Bool(true)),
                    "false" => Ok(Val::Bool(false)),
                    _ => {
                        self.i = save;
                        let p = self.pred()?;
                        Ok(Val::Entity(Box::new(p)))
                    }
                }
            }
            other => Err(format!("unexpected {:?} at {}", other.map(|c| c as char), self.i)),
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
    fn nested_entity_is_a_value() {
        let p = Pred::parse("invoice(customer=customer(name='Acme')).exists").unwrap();
        match p.arg("customer") {
            Some(Val::Entity(inner)) => assert_eq!(inner.entity, "customer"),
            other => panic!("{other:?}"),
        }
        assert!(p.is_existence());
    }
}
