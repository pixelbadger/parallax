//! Runtime values. Every value is immutable to the program; arrays and
//! records are shared and copied on write (`Rc::make_mut`) when a local
//! `var` is updated in place.

use std::rc::Rc;

#[derive(Clone, Debug, Default)]
pub enum Value {
    #[default]
    Unit,
    Bool(bool),
    Int(i64),
    /// A float or a quantity, in its dimension's base unit.
    Num(f64),
    Str(Rc<String>),
    /// A plain enum value: its variant's index.
    Variant(u32),
    /// An enum value with fields: variant index and payload.
    Data(Rc<(u32, Vec<Value>)>),
    Rec(Rc<Vec<Value>>),
    Arr(Rc<Vec<Value>>),
}

const _: () = assert!(std::mem::size_of::<Value>() == 16);

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Unit, Value::Unit) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Num(a), Value::Num(b)) => a == b,
            (Value::Int(a), Value::Num(b)) | (Value::Num(b), Value::Int(a)) => *a as f64 == *b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Variant(a), Value::Variant(b)) => a == b,
            (Value::Data(a), Value::Data(b)) => a == b,
            (Value::Rec(a), Value::Rec(b)) | (Value::Arr(a), Value::Arr(b)) => a == b,
            _ => false,
        }
    }
}

impl Value {
    pub fn arr(v: Vec<Value>) -> Value {
        Value::Arr(Rc::new(v))
    }

    pub fn variant(tag: u32, fields: Vec<Value>) -> Value {
        if fields.is_empty() {
            Value::Variant(tag)
        } else {
            Value::Data(Rc::new((tag, fields)))
        }
    }

    /// An enum value's variant and fields.
    pub fn as_variant(&self) -> Option<(u32, &[Value])> {
        match self {
            Value::Variant(t) => Some((*t, &[])),
            Value::Data(d) => Some((d.0, &d.1)),
            _ => None,
        }
    }

    pub fn as_int(&self) -> i64 {
        match self {
            Value::Int(n) => *n,
            Value::Num(x) => *x as i64,
            Value::Bool(b) => *b as i64,
            _ => 0,
        }
    }

    pub fn as_num(&self) -> f64 {
        match self {
            Value::Int(n) => *n as f64,
            Value::Num(x) => *x,
            Value::Bool(b) => *b as i64 as f64,
            _ => f64::NAN,
        }
    }

    pub fn as_bool(&self) -> bool {
        matches!(self, Value::Bool(true))
    }

    pub fn items(&self) -> &[Value] {
        match self {
            Value::Arr(a) | Value::Rec(a) => a,
            _ => &[],
        }
    }
}
