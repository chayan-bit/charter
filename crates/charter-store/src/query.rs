//! Typed query builder rendered to the Arkiv query language (design 7.2, 4.8).
//!
//! There is deliberately no `Not` and no `!=`: GolemDB rejects both (conformance Q1, Q4).

use std::fmt::Write as _;

use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};

use crate::StoreError;

/// Longest query text the node accepts (`arkiv-query` limits).
pub const MAX_QUERY_BYTES: usize = 8 * 1024;
/// Most predicates in one query.
pub const MAX_PREDICATES: usize = 64;
/// Most nested `AND`/`OR` levels.
pub const MAX_AND_OR_DEPTH: usize = 4;
/// Largest page the node serves.
pub const MAX_LIMIT: u32 = 100;
/// Longest `str` literal.
pub const MAX_STR_BYTES: usize = 128;
/// Longest attribute name.
pub const MAX_NAME_BYTES: usize = 32;

/// What a predicate tests: a user attribute or a system attribute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Field {
    Attr(String),
    Owner,
    Creator,
    Key,
    ExpiresAt,
}

/// A user attribute by name.
pub fn attr(name: &str) -> Field {
    Field::Attr(name.to_string())
}

/// A typed literal. Only the types Charter queries are offered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Value {
    Str(String),
    U64(u64),
    Bytes32(B256),
    Addr(Address),
    Key(B256),
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Self::Str(s.to_string())
    }
}
impl From<u64> for Value {
    fn from(n: u64) -> Self {
        Self::U64(n)
    }
}
impl From<B256> for Value {
    fn from(b: B256) -> Self {
        Self::Bytes32(b)
    }
}
impl From<Address> for Value {
    fn from(a: Address) -> Self {
        Self::Addr(a)
    }
}

/// The predicate tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pred {
    All,
    Eq(Field, Value),
    Ge(Field, Value),
    Le(Field, Value),
    StartsWith(Field, String),
    And(Vec<Pred>),
    Or(Vec<Pred>),
}

impl Pred {
    pub fn eq(f: Field, v: impl Into<Value>) -> Self {
        Self::Eq(f, v.into())
    }
    pub fn ge(f: Field, v: impl Into<Value>) -> Self {
        Self::Ge(f, v.into())
    }
    pub fn le(f: Field, v: impl Into<Value>) -> Self {
        Self::Le(f, v.into())
    }
}

/// The type an `ORDER BY` reads its attribute as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderType {
    U64,
    Str,
    Bytes32,
}

/// `ORDER BY field:type [DESC]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderBy {
    pub field: Field,
    pub ty: OrderType,
    pub desc: bool,
}

/// A full query: predicates plus an optional sort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Query {
    pub filter: Pred,
    pub order: Option<OrderBy>,
}

impl From<Pred> for Query {
    fn from(filter: Pred) -> Self {
        Self {
            filter,
            order: None,
        }
    }
}

impl Query {
    /// Sort ascending by a `u64` attribute (seq, chunk index).
    pub fn order_by_u64(mut self, name: &str) -> Self {
        self.order = Some(OrderBy {
            field: attr(name),
            ty: OrderType::U64,
            desc: false,
        });
        self
    }

    /// Render to the node's query string, refusing anything the node would refuse.
    pub fn render(&self) -> Result<String, StoreError> {
        let mut out = String::new();
        let mut preds = 0;
        render_pred(&self.filter, &mut out, &mut preds, 0)?;
        if preds > MAX_PREDICATES {
            return Err(invalid(format!("{preds} predicates, max {MAX_PREDICATES}")));
        }
        if let Some(o) = &self.order {
            let ty = match o.ty {
                OrderType::U64 => "u64",
                OrderType::Str => "str",
                OrderType::Bytes32 => "bytes32",
            };
            let dir = if o.desc { "DESC" } else { "ASC" };
            let _ = write!(out, " ORDER BY {}:{ty} {dir}", field(&o.field)?);
        }
        if out.len() > MAX_QUERY_BYTES {
            return Err(invalid(format!(
                "{} B of query text, max {MAX_QUERY_BYTES}",
                out.len()
            )));
        }
        Ok(out)
    }
}

fn invalid(msg: String) -> StoreError {
    StoreError::InvalidQuery(msg)
}

fn render_pred(
    p: &Pred,
    out: &mut String,
    preds: &mut usize,
    depth: usize,
) -> Result<(), StoreError> {
    let (op, f, v) = match p {
        Pred::All => {
            out.push('*');
            return Ok(());
        }
        Pred::And(xs) => return render_chain(xs, " AND ", out, preds, depth),
        Pred::Or(xs) => return render_chain(xs, " OR ", out, preds, depth),
        Pred::Eq(f, v) => ("=", f, value(v)?),
        Pred::Ge(f, v) => (">=", f, value(v)?),
        Pred::Le(f, v) => ("<=", f, value(v)?),
        Pred::StartsWith(f, s) => ("STARTSWITH", f, value(&Value::Str(s.clone()))?),
    };
    let fits = match (f, p) {
        (Field::Attr(_), _) => true,
        (_, Pred::StartsWith(..)) => false,
        (Field::Key, Pred::Eq(_, x) | Pred::Ge(_, x) | Pred::Le(_, x)) => {
            matches!(x, Value::Key(_))
        }
        (Field::Owner | Field::Creator, Pred::Eq(_, x) | Pred::Ge(_, x) | Pred::Le(_, x)) => {
            matches!(x, Value::Addr(_))
        }
        (Field::ExpiresAt, Pred::Eq(_, x) | Pred::Ge(_, x) | Pred::Le(_, x)) => {
            matches!(x, Value::U64(_))
        }
        _ => false,
    };
    if !fits {
        return Err(invalid(format!(
            "{} cannot be compared with {v}",
            field(f)?
        )));
    }
    *preds += 1;
    let _ = write!(out, "{} {op} {v}", field(f)?);
    Ok(())
}

fn render_chain(
    xs: &[Pred],
    sep: &str,
    out: &mut String,
    preds: &mut usize,
    depth: usize,
) -> Result<(), StoreError> {
    // `*` adds nothing to an AND; an OR containing `*` is `*`.
    let xs: Vec<&Pred> = xs.iter().filter(|x| !matches!(x, Pred::All)).collect();
    match xs.as_slice() {
        [] if sep == " AND " => return render_pred(&Pred::All, out, preds, depth),
        [] => return Err(invalid("empty OR matches nothing".into())),
        [one] => return render_pred(one, out, preds, depth),
        _ => {}
    }
    if depth + 1 > MAX_AND_OR_DEPTH {
        return Err(invalid(format!(
            "AND/OR nesting deeper than {MAX_AND_OR_DEPTH}"
        )));
    }
    for (i, x) in xs.iter().enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        let nested = matches!(x, Pred::And(_) | Pred::Or(_));
        if nested {
            out.push('(');
        }
        render_pred(x, out, preds, depth + 1)?;
        if nested {
            out.push(')');
        }
    }
    Ok(())
}

fn field(f: &Field) -> Result<String, StoreError> {
    Ok(match f {
        Field::Attr(n) => {
            let ok = n.len() <= MAX_NAME_BYTES
                && n.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
                && n.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b));
            if !ok {
                return Err(invalid(format!("bad attribute name {n:?}")));
            }
            n.clone()
        }
        Field::Owner => "$owner".into(),
        Field::Creator => "$creator".into(),
        Field::Key => "$key".into(),
        Field::ExpiresAt => "$expiresAt".into(),
    })
}

fn value(v: &Value) -> Result<String, StoreError> {
    Ok(match v {
        Value::Str(s) if s.len() > MAX_STR_BYTES => {
            return Err(invalid(format!(
                "str literal of {} B, max {MAX_STR_BYTES}",
                s.len()
            )));
        }
        Value::Str(s) => format!("str('{}')", s.replace('\'', "''")),
        Value::U64(n) => format!("u64({n})"),
        Value::Bytes32(b) => format!("bytes32({b:#x})"),
        Value::Key(b) => format!("key({b:#x})"),
        // All-lowercase carries no EIP-55 checksum, which the parser accepts.
        Value::Addr(a) => format!("addr({})", alloy_primitives::hex::encode_prefixed(a)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(k: &str) -> Pred {
        Pred::eq(attr("ag.kind"), k)
    }

    #[test]
    fn renders_the_design_queries() {
        let rid = B256::repeat_byte(0xab);
        let a = Address::repeat_byte(0x11);
        let b = Address::repeat_byte(0x22);
        let q = Query::from(Pred::And(vec![
            kind("anchor"),
            Pred::eq(attr("ag.rid"), rid),
            Pred::Or(vec![
                Pred::eq(Field::Creator, a),
                Pred::eq(Field::Creator, b),
            ]),
            Pred::ge(attr("ag.seq"), 7u64),
        ]))
        .order_by_u64("ag.seq");
        let s = q.render().expect("renders");
        assert_eq!(
            s,
            format!(
                "ag.kind = str('anchor') AND ag.rid = bytes32(0x{}) AND \
                 ($creator = addr(0x{}) OR $creator = addr(0x{})) AND ag.seq >= u64(7) \
                 ORDER BY ag.seq:u64 ASC",
                "ab".repeat(32),
                "11".repeat(20),
                "22".repeat(20)
            )
        );
        assert!(!s.contains("NOT") && !s.contains("!="));
    }

    #[test]
    fn escapes_quotes_and_simplifies_trivial_chains() {
        let q = Query::from(Pred::StartsWith(attr("ag.name"), "it's".into()));
        assert_eq!(q.render().expect("ok"), "ag.name STARTSWITH str('it''s')");
        assert_eq!(Query::from(Pred::And(vec![])).render().expect("ok"), "*");
        assert_eq!(
            Query::from(Pred::And(vec![Pred::All, kind("x")]))
                .render()
                .expect("ok"),
            "ag.kind = str('x')"
        );
        let keyq = Query::from(Pred::Le(Field::ExpiresAt, Value::U64(9)));
        assert_eq!(keyq.render().expect("ok"), "$expiresAt <= u64(9)");
        let k = Query::from(Pred::eq(Field::Key, Value::Key(B256::ZERO)));
        assert!(k.render().expect("ok").starts_with("$key = key(0x0000"));
    }

    #[test]
    fn refuses_what_the_node_refuses() {
        let too_deep = Pred::And(vec![
            kind("a"),
            Pred::Or(vec![
                kind("b"),
                Pred::And(vec![
                    kind("c"),
                    Pred::Or(vec![kind("d"), Pred::And(vec![kind("e"), kind("f")])]),
                ]),
            ]),
        ]);
        assert_eq!(
            Query::from(too_deep).render().map_err(|e| e.code()),
            Err("E_INTERNAL")
        );
        let many = Pred::Or((0..65u64).map(|i| Pred::eq(attr("ag.i"), i)).collect());
        assert!(Query::from(many).render().is_err());
        assert!(Query::from(Pred::Or(vec![])).render().is_err());
        assert!(Query::from(Pred::eq(attr("1bad"), 1u64)).render().is_err());
        assert!(Query::from(Pred::eq(attr("a b"), 1u64)).render().is_err());
        assert!(
            Query::from(Pred::eq(attr(&"n".repeat(33)), 1u64))
                .render()
                .is_err()
        );
        assert!(
            Query::from(Pred::eq(attr("s"), "x".repeat(129).as_str()))
                .render()
                .is_err()
        );
        let quotes = "'".repeat(MAX_STR_BYTES);
        let long = Pred::Or(
            (0..40)
                .map(|_| Pred::eq(attr("ag.name"), quotes.as_str()))
                .collect(),
        );
        assert!(Query::from(long).render().is_err(), "over 8 KiB");
        for bad in [
            Pred::eq(Field::Key, B256::ZERO),
            Pred::eq(Field::Owner, 1u64),
            Pred::eq(Field::Creator, B256::ZERO),
            Pred::ge(Field::ExpiresAt, Address::ZERO),
            Pred::StartsWith(Field::Owner, "0x".into()),
        ] {
            assert!(Query::from(bad.clone()).render().is_err(), "{bad:?}");
        }
    }

    /// Every rendered query parses with the node's own parser (design 8.1).
    #[cfg(feature = "fake")]
    #[test]
    fn rendered_queries_parse_with_arkiv_query() {
        let qs = [
            Query::from(Pred::And(vec![
                kind("chunk"),
                Pred::ge(attr("ag.i"), 0u64),
                Pred::le(attr("ag.i"), 15u64),
            ]))
            .order_by_u64("ag.i"),
            Query::from(Pred::And(vec![
                Pred::eq(attr("ag.rid"), B256::repeat_byte(1)),
                Pred::Or(vec![kind("id"), kind("idacc")]),
            ])),
            Query::from(Pred::eq(Field::Creator, Address::repeat_byte(0xAB))),
            Query::from(Pred::StartsWith(attr("ag.name"), "o'k".into())),
            Query::from(Pred::eq(Field::Key, Value::Key(B256::repeat_byte(3)))),
            Query::from(Pred::All),
        ];
        for q in qs {
            let s = q.render().expect("renders");
            arkiv_query::parse_statement(&s).unwrap_or_else(|e| panic!("{s}: {e:?}"));
        }
    }
}
