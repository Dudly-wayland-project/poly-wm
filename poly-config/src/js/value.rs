//! An engine-independent value tree for configuration data.
//!
//! Scripts are executed by QuickJS, but nothing downstream of the loader knows
//! that: every global helper converts the JavaScript values it is handed into
//! this tree, and the parser only ever sees [`Value`]/[`Table`]. That keeps the
//! validation code free of engine lifetimes and lets repeated `config()` calls
//! be merged as plain data.
//!
//! The shape is deliberately close to the Lua tables the parser was originally
//! written against: tables are ordered key/value lists, and an array is a table
//! whose keys are `1 ..= len`. [`Table`] is a cheap handle to shared, mutable
//! storage so that `config()` calls can be merged in place.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::ops::Deref;
use std::rc::Rc;

/// How deep [`super::convert`] follows nested objects.
///
/// Deeper values are reported as an error instead of recursing forever, which
/// also makes a cyclic object literal terminate.
pub(crate) const MAX_VALUE_DEPTH: usize = 64;

/// A string from the script engine.
///
/// JavaScript strings are UTF-16 internally; the loader converts them to Rust
/// strings at the boundary (invalid ones are reported there), so everything
/// that reaches the parser is valid UTF-8.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JsString(Rc<str>);

impl JsString {
    /// Borrow the text.
    pub fn as_str(&self) -> &str {
        return &self.0;
    }
}

impl From<String> for JsString {
    fn from(text: String) -> Self {
        return Self(Rc::from(text));
    }
}

impl From<&str> for JsString {
    fn from(text: &str) -> Self {
        return Self(Rc::from(text));
    }
}

impl Deref for JsString {
    type Target = str;

    fn deref(&self) -> &str {
        return &self.0;
    }
}

impl PartialEq<str> for JsString {
    fn eq(&self, other: &str) -> bool {
        return self.as_str() == other;
    }
}

impl PartialEq<&str> for JsString {
    fn eq(&self, other: &&str) -> bool {
        return self.as_str() == *other;
    }
}

impl fmt::Display for JsString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        return f.write_str(&self.0);
    }
}

/// A configuration value, converted from JavaScript.
#[derive(Clone, Debug)]
pub enum Value {
    /// `undefined` and `null`; never stored in a [`Table`].
    Nil,
    /// A boolean.
    Boolean(bool),
    /// An integer that fits in an `i64`.
    Integer(i64),
    /// A floating point number.
    Number(f64),
    /// A string.
    String(JsString),
    /// An object or array.
    Table(Table),
    /// A function; kept only so error messages can name the type.
    Function,
    /// Anything else (symbols, promises, …), named for error messages.
    Other(&'static str),
}

impl Value {
    /// A string value.
    pub fn string(text: impl Into<JsString>) -> Self {
        return Self::String(text.into());
    }

    /// The table behind this value.
    pub fn as_table(&self) -> Option<&Table> {
        return match self {
            Value::Table(table) => Some(table),
            _ => None,
        };
    }

    /// The value as a number, when it is one.
    pub fn as_f64(&self) -> Option<f64> {
        return match self {
            Value::Integer(number) => Some(*number as f64),
            Value::Number(number) => Some(*number),
            _ => None,
        };
    }

    /// The value as an integer, when it is an integral number.
    pub fn as_i64(&self) -> Option<i64> {
        return match self {
            Value::Integer(number) => Some(*number),
            Value::Number(number) if number.fract() == 0.0 && number.is_finite() => {
                Some(*number as i64)
            }
            _ => None,
        };
    }

    /// The JavaScript type name, for error messages.
    pub fn type_name(&self) -> &'static str {
        return match self {
            Value::Nil => "nil",
            Value::Boolean(_) => "boolean",
            Value::Integer(_) | Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Table(table) if table.is_array() => "array",
            Value::Table(_) => "object",
            Value::Function => "function",
            Value::Other(name) => name,
        };
    }
}

/// An ordered key/value table.
///
/// Cloning shares the storage; the loader builds the tree once and the parser
/// only reads it.
#[derive(Clone, Default)]
pub struct Table {
    entries: Rc<RefCell<Vec<(Value, Value)>>>,
    /// Set when the table was built from a JavaScript array, so an empty `[]`
    /// stays distinguishable from an empty `{}`.
    array: Cell<bool>,
}

impl Table {
    /// An empty table.
    pub fn new() -> Self {
        return Self::default();
    }

    /// An empty table that came from an array literal.
    pub fn new_array() -> Self {
        let table = Self::default();
        table.array.set(true);
        return table;
    }

    /// A snapshot of the entries.
    pub fn entries(&self) -> Vec<(Value, Value)> {
        return self.entries.borrow().clone();
    }

    /// The value stored under a string key.
    pub fn get(&self, key: &str) -> Option<Value> {
        let entries = self.entries.borrow();
        return entries
            .iter()
            .find(|(stored, _)| {
                return matches!(stored, Value::String(text) if text.as_str() == key);
            })
            .map(|(_, value)| return value.clone());
    }

    /// The value stored at a 1-based array index.
    pub fn get_index(&self, index: usize) -> Option<Value> {
        let entries = self.entries.borrow();
        let key = index as i64;
        return entries
            .iter()
            .find(|(stored, _)| return matches!(stored, Value::Integer(value) if *value == key))
            .map(|(_, value)| return value.clone());
    }

    /// Store a value under a string key, replacing an existing entry.
    pub fn set(&self, key: impl Into<String>, value: Value) {
        let key = Value::string(key.into());
        let mut entries = self.entries.borrow_mut();
        for (stored, existing) in entries.iter_mut() {
            if *stored == key {
                *existing = value;
                return;
            }
        }
        entries.push((key, value));
    }

    /// Store a value under a 1-based array index.
    pub fn set_index(&self, index: usize, value: Value) {
        let key = Value::Integer(index as i64);
        let mut entries = self.entries.borrow_mut();
        for (stored, existing) in entries.iter_mut() {
            if *stored == key {
                *existing = value;
                return;
            }
        }
        entries.push((key, value));
    }

    /// Whether this table is an array.
    ///
    /// A table built from `[]` says so explicitly; one built by the helpers
    /// (`append`, `set_index`) is recognised by its keys being exactly
    /// `1 ..= len`.
    pub fn is_array(&self) -> bool {
        if self.array.get() {
            return true;
        }
        let entries = self.entries.borrow();
        if entries.is_empty() {
            return false;
        }
        for (index, (key, _)) in entries.iter().enumerate() {
            let Value::Integer(value) = key else {
                return false;
            };
            if *value != index as i64 + 1 {
                return false;
            }
        }
        return true;
    }

    /// Every array element, in index order.
    pub fn sequence_values(&self) -> Vec<Value> {
        let entries = self.entries.borrow();
        let mut values: Vec<(i64, Value)> = Vec::new();
        for (key, value) in entries.iter() {
            if let Value::Integer(index) = key {
                values.push((*index, value.clone()));
            }
        }
        values.sort_by_key(|(index, _)| return *index);
        return values
            .into_iter()
            .filter(|(index, _)| return *index >= 1)
            .map(|(_, value)| return value)
            .collect();
    }

    /// Deep-merge `overlay` over this table.
    ///
    /// Objects merge key by key; arrays and scalars replace the old value
    /// wholesale. That is what makes a second `config()` call able to recolour
    /// a border without losing its width, while `window_rules` stays a single
    /// list.
    pub fn merge_over(&self, overlay: &Table) {
        for (key, value) in overlay.entries() {
            let existing = match key {
                Value::String(ref text) => self.get(text.as_str()),
                Value::Integer(index) => self.get_index(index as usize),
                _ => None,
            };
            match (existing, value) {
                (Some(Value::Table(mine)), Value::Table(theirs))
                    if !mine.is_array() && !theirs.is_array() =>
                {
                    mine.merge_over(&theirs);
                }
                (_, value) => match key {
                    Value::String(text) => self.set(text.as_str(), value),
                    Value::Integer(index) => self.set_index(index as usize, value),
                    _ => {}
                },
            }
        }
    }
}

impl fmt::Debug for Table {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        return f.debug_map().entries(self.entries()).finish();
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        return match (self, other) {
            (Value::Nil, Value::Nil) => true,
            (Value::Boolean(left), Value::Boolean(right)) => left == right,
            (Value::Integer(left), Value::Integer(right)) => left == right,
            (Value::Number(left), Value::Number(right)) => left == right,
            (Value::Integer(left), Value::Number(right))
            | (Value::Number(right), Value::Integer(left)) => *right == *left as f64,
            (Value::String(left), Value::String(right)) => left == right,
            (Value::Table(left), Value::Table(right)) => Rc::ptr_eq(&left.entries, &right.entries),
            (Value::Function, Value::Function) => true,
            (Value::Other(left), Value::Other(right)) => left == right,
            _ => false,
        };
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        return Value::Boolean(value);
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        return Value::Integer(value);
    }
}

impl From<u64> for Value {
    fn from(value: u64) -> Self {
        return Value::Integer(value as i64);
    }
}

impl From<usize> for Value {
    fn from(value: usize) -> Self {
        return Value::Integer(value as i64);
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        return Value::Number(value);
    }
}

impl From<i32> for Value {
    fn from(value: i32) -> Self {
        return Value::Integer(value as i64);
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        return Value::string(value);
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        return Value::string(value);
    }
}

impl From<Table> for Value {
    fn from(value: Table) -> Self {
        return Value::Table(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrays_are_tables_keyed_from_one() {
        let table = Table::new();
        assert!(!table.is_array());
        table.set_index(1, Value::from(1));
        table.set_index(2, Value::from(2));
        assert!(table.is_array());
        assert_eq!(
            table.sequence_values(),
            vec![Value::from(1), Value::from(2)]
        );

        table.set("name", Value::from("mixed"));
        assert!(!table.is_array());
    }

    #[test]
    fn merging_keeps_sibling_keys_and_replaces_arrays() {
        let base = Table::new();
        let border = Table::new();
        border.set("width", Value::from(2));
        border.set("active", Value::from("#ffffff"));
        base.set("border", Value::Table(border));

        let rules = Table::new();
        rules.set_index(1, Value::from("old"));
        base.set("window_rules", Value::Table(rules));

        let overlay = Table::new();
        let border = Table::new();
        border.set("active", Value::from("#000000"));
        overlay.set("border", Value::Table(border));
        let rules = Table::new();
        rules.set_index(1, Value::from("new"));
        overlay.set("window_rules", Value::Table(rules));

        base.merge_over(&overlay);

        let border = base.get("border").unwrap();
        let border = border.as_table().unwrap();
        assert_eq!(border.get("width").unwrap(), Value::from(2));
        assert_eq!(border.get("active").unwrap(), Value::from("#000000"));

        let rules = base.get("window_rules").unwrap();
        assert_eq!(
            rules.as_table().unwrap().sequence_values(),
            vec![Value::from("new")]
        );
    }
}
