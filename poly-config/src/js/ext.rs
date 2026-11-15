//! Reading typed values out of [`Table`]s, collecting every problem instead of
//! bailing out on the first one.

use crate::error::{Errors, path_index, path_key};
use crate::js::value::{Table, Value};

/// A field of a table, or `None` when it is absent.
pub(crate) fn field(table: &Table, key: &str) -> Option<Value> {
    return table.get(key);
}

/// A string field.
pub(crate) fn string(table: &Table, key: &str, base: &str, errors: &mut Errors) -> Option<String> {
    let path = path_key(base, key);
    let value = field(table, key)?;
    return match value {
        Value::String(text) => Some(text.as_str().to_owned()),
        value => {
            errors.error(
                path,
                format!("expected a string, got {}", value.type_name()),
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
        Value::Boolean(value) => Some(value),
        value => {
            errors.error(
                path,
                format!("expected a boolean, got {}", value.type_name()),
            );
            None
        }
    };
}

/// A field that must be a finite number.
pub(crate) fn number(table: &Table, key: &str, base: &str, errors: &mut Errors) -> Option<f64> {
    let path = path_key(base, key);
    let value = field(table, key)?;
    let number = match value.as_f64() {
        Some(number) => number,
        None => {
            errors.error(
                path,
                format!("expected a number, got {}", value.type_name()),
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
    let number = match value.as_i64() {
        Some(number) => number,
        None => {
            errors.error(
                path,
                format!("expected an integer, got {}", value.type_name()),
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

/// An object field.
pub(crate) fn table(table: &Table, key: &str, base: &str, errors: &mut Errors) -> Option<Table> {
    let path = path_key(base, key);
    let value = field(table, key)?;
    return match value {
        Value::Table(value) => Some(value),
        value => {
            errors.error(
                path,
                format!("expected an object, got {}", value.type_name()),
            );
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
            format!("expected a list of strings, got {}", value.type_name()),
        );
        return None;
    };
    if !list.is_array() {
        errors.error(
            path,
            format!("expected a list of strings, got {}", value.type_name()),
        );
        return None;
    }

    let mut strings = Vec::new();
    for (index, item) in list.sequence_values().iter().enumerate() {
        let item_path = path_index(path, index + 1);
        match item {
            Value::String(text) => strings.push(text.as_str().to_owned()),
            other => errors.error(
                item_path,
                format!("expected a string, got {}", other.type_name()),
            ),
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
            format!("expected a list of numbers, got {}", value.type_name()),
        );
        return None;
    };

    let mut numbers = Vec::new();
    for (index, value) in list.sequence_values().iter().enumerate() {
        let item_path = path_index(&path, index + 1);
        match value.as_f64() {
            Some(number) => numbers.push(number as f32),
            None => errors.error(
                item_path,
                format!("expected a number, got {}", value.type_name()),
            ),
        }
    }
    return Some(numbers);
}

/// Report every key of an object that is not in `known` as a warning.
pub(crate) fn warn_unknown(table: &Table, base: &str, known: &[&str], errors: &mut Errors) {
    for (key, _value) in table.entries() {
        let Value::String(key) = key else {
            errors.warn(
                base,
                format!("expected string keys, got {}", key.type_name()),
            );
            continue;
        };
        if !known.contains(&key.as_str()) {
            errors.warn(
                path_key(base, key.as_str()),
                "unknown setting, it will be ignored",
            );
        }
    }
}

/// Iterate the string-keyed entries of an object.
pub(crate) fn string_entries(
    table: &Table,
    base: &str,
    errors: &mut Errors,
) -> Vec<(String, Value)> {
    let mut entries = Vec::new();
    for (key, value) in table.entries() {
        let Value::String(key) = key else {
            errors.error(
                base,
                format!("expected string keys, got {}", key.type_name()),
            );
            continue;
        };
        entries.push((key.as_str().to_owned(), value));
    }
    return entries;
}
