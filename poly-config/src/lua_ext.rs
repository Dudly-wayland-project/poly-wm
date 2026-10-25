//! Reading typed values out of Lua tables, collecting every problem instead of
//! bailing out on the first one.

use mlua::{Table, Value};

use crate::error::{Errors, path_key};

/// The Lua type name of a value, for error messages.
pub(crate) fn type_name(value: &Value) -> &'static str {
    return match value {
        Value::Nil => "nil",
        Value::Boolean(_) => "boolean",
        Value::LightUserData(_) => "userdata",
        Value::Integer(_) => "number",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Table(_) => "table",
        Value::Function(_) => "function",
        Value::Thread(_) => "thread",
        Value::UserData(_) => "userdata",
        Value::Error(_) => "error",
        _ => "value",
    };
}

/// A field of a table, or `None` when it is nil/absent.
pub(crate) fn field(table: &Table, key: &str) -> Option<Value> {
    let value = table.get::<Value>(key).ok()?;
    return match value {
        Value::Nil => None,
        value => Some(value),
    };
}

/// A string field.
pub(crate) fn string(table: &Table, key: &str, base: &str, errors: &mut Errors) -> Option<String> {
    let path = path_key(base, key);
    let value = field(table, key)?;
    return match value {
        Value::String(s) => match s.to_str() {
            Ok(s) => Some(s.to_owned()),
            Err(_) => {
                errors.error(path, "string is not valid UTF-8");
                None
            }
        },
        value => {
            errors.error(
                path,
                format!("expected a string, got {}", type_name(&value)),
            );
            None
        }
    };
}

/// A boolean field.
pub(crate) fn boolean(table: &Table, key: &str, base: &str, errors: &mut Errors) -> Option<bool> {
    let path = path_key(base, key);
    let value = field(table, key)?;
    return match value {
        Value::Boolean(b) => Some(b),
        value => {
            errors.error(
                path,
                format!("expected a boolean, got {}", type_name(&value)),
            );
            None
        }
    };
}

/// A field that must be a finite number.
pub(crate) fn number(table: &Table, key: &str, base: &str, errors: &mut Errors) -> Option<f64> {
    let path = path_key(base, key);
    let value = field(table, key)?;
    let number = match value {
        Value::Integer(i) => i as f64,
        Value::Number(f) => f,
        value => {
            errors.error(
                path,
                format!("expected a number, got {}", type_name(&value)),
            );
            return None;
        }
    };
    if !number.is_finite() {
        errors.error(path, "number must be finite");
        return None;
    }
    return Some(number);
}

/// A field that must be an integer in range.
pub(crate) fn integer(
    table: &Table,
    key: &str,
    base: &str,
    min: i64,
    max: i64,
    errors: &mut Errors,
) -> Option<i64> {
    let path = path_key(base, key);
    let value = field(table, key)?;
    let number = match value {
        Value::Integer(i) => i,
        Value::Number(f) if f.fract() == 0.0 && f.is_finite() => f as i64,
        value => {
            errors.error(
                path,
                format!("expected an integer, got {}", type_name(&value)),
            );
            return None;
        }
    };
    if number < min || number > max {
        errors.error(path, format!("value must be between {min} and {max}"));
        return None;
    }
    return Some(number);
}

/// A table field.
pub(crate) fn table(table: &Table, key: &str, base: &str, errors: &mut Errors) -> Option<Table> {
    let path = path_key(base, key);
    let value = field(table, key)?;
    return match value {
        Value::Table(t) => Some(t),
        value => {
            errors.error(path, format!("expected a table, got {}", type_name(&value)));
            None
        }
    };
}

/// An array-of-strings field.
pub(crate) fn string_list(
    table: &Table,
    key: &str,
    base: &str,
    errors: &mut Errors,
) -> Option<Vec<String>> {
    let path = path_key(base, key);
    let value = field(table, key)?;
    return string_list_value(&value, &path, errors);
}

/// A value that must be an array of strings.
pub(crate) fn string_list_value(
    value: &Value,
    path: &str,
    errors: &mut Errors,
) -> Option<Vec<String>> {
    let Value::Table(list) = value else {
        errors.error(
            path,
            format!("expected a list of strings, got {}", type_name(value)),
        );
        return None;
    };

    let mut strings = Vec::new();
    for (index, item) in list.clone().sequence_values::<Value>().enumerate() {
        let item_path = crate::error::path_index(path, index + 1);
        match item {
            Ok(Value::String(s)) => match s.to_str() {
                Ok(s) => strings.push(s.to_owned()),
                Err(_) => errors.error(item_path, "string is not valid UTF-8"),
            },
            Ok(other) => errors.error(
                item_path,
                format!("expected a string, got {}", type_name(&other)),
            ),
            Err(err) => errors.error(item_path, err.to_string()),
        }
    }
    return Some(strings);
}

/// A list of numbers (used for calibration matrices).
pub(crate) fn number_list(
    table: &Table,
    key: &str,
    base: &str,
    errors: &mut Errors,
) -> Option<Vec<f32>> {
    let path = path_key(base, key);
    let value = field(table, key)?;
    let Value::Table(list) = value else {
        errors.error(
            path,
            format!("expected a list of numbers, got {}", type_name(&value)),
        );
        return None;
    };

    let mut numbers = Vec::new();
    for (index, value) in list.sequence_values::<Value>().enumerate() {
        let item_path = crate::error::path_index(&path, index + 1);
        match value {
            Ok(Value::Integer(i)) => numbers.push(i as f32),
            Ok(Value::Number(f)) => numbers.push(f as f32),
            Ok(value) => errors.error(
                item_path,
                format!("expected a number, got {}", type_name(&value)),
            ),
            Err(err) => errors.error(item_path, err.to_string()),
        }
    }
    return Some(numbers);
}

/// Report every key of a table that is not in `known` as a warning.
pub(crate) fn warn_unknown(table: &Table, base: &str, known: &[&str], errors: &mut Errors) {
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _value) = match pair {
            Ok(pair) => pair,
            Err(err) => {
                errors.error(base, err.to_string());
                return;
            }
        };
        let Value::String(key) = key else {
            errors.warn(
                base,
                format!("expected string keys, got {}", type_name(&key)),
            );
            continue;
        };
        let Ok(key) = key.to_str() else {
            errors.warn(base, "key is not valid UTF-8");
            continue;
        };
        if !known.contains(&key.as_ref()) {
            errors.warn(path_key(base, &key), "unknown setting, it will be ignored");
        }
    }
}

/// Iterate the string-keyed entries of a table, or report why that failed.
pub(crate) fn string_entries(
    table: &Table,
    base: &str,
    errors: &mut Errors,
) -> Vec<(String, Value)> {
    let mut entries = Vec::new();
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, value) = match pair {
            Ok(pair) => pair,
            Err(err) => {
                errors.error(base, err.to_string());
                return entries;
            }
        };
        let Value::String(key) = key else {
            errors.error(
                base,
                format!("expected string keys, got {}", type_name(&key)),
            );
            continue;
        };
        let Ok(key) = key.to_str() else {
            errors.error(base, "key is not valid UTF-8");
            continue;
        };
        entries.push((key.to_owned(), value));
    }
    return entries;
}
