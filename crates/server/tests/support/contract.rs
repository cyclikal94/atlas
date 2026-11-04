//! A JSON Schema check against `api/openapi.json` for the keywords the contract's response
//! schemas use, shared by the HTTP cases that validate `200` bodies. An unsupported keyword that
//! applies to a value fails loudly rather than passing silently. Its negative controls are in
//! `cases::people_summary`.
use anyhow::Result;
use serde_json::{Value, json};

pub(crate) fn contract() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../api/openapi.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

const UUID_PATTERN: &str = "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$";

/// A JSON Schema check for the keywords these responses use. An unsupported keyword that applies
/// to a value fails loudly rather than passing silently.
pub(crate) fn check(
    contract: &Value,
    schema: &Value,
    value: &Value,
    at: &str,
) -> Result<(), String> {
    let fail = |why: String| Err(format!("{at}: {why} in {value}"));
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let name = reference.trim_start_matches("#/components/schemas/");
        return check(
            contract,
            &contract["components"]["schemas"][name],
            value,
            at,
        );
    }
    if let Some(kind) = schema.get("type") {
        let kinds: Vec<&str> = match kind {
            Value::String(k) => vec![k.as_str()],
            Value::Array(ks) => ks.iter().filter_map(Value::as_str).collect(),
            _ => return fail("bad type".into()),
        };
        let matches = kinds.iter().any(|k| match *k {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => false,
        });
        if !matches {
            return fail(format!("not {kinds:?}"));
        }
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array)
        && !options.contains(value)
    {
        return fail(format!("not one of {options:?}"));
    }
    if let Some(constant) = schema.get("const")
        && constant != value
    {
        return fail(format!("not {constant}"));
    }
    if let Some(s) = value.as_str() {
        if schema.get("format").and_then(Value::as_str) == Some("uuid")
            && uuid::Uuid::parse_str(s).map(|u| u.to_string()) != Ok(s.to_owned())
        {
            return fail("not a canonical uuid".into());
        }
        if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
            assert_eq!(pattern, UUID_PATTERN, "unsupported pattern at {at}");
        }
        let length = s.chars().count() as u64;
        if schema
            .get("minLength")
            .and_then(Value::as_u64)
            .is_some_and(|m| length < m)
            || schema
                .get("maxLength")
                .and_then(Value::as_u64)
                .is_some_and(|m| length > m)
        {
            return fail("length out of range".into());
        }
    }
    if let Some(n) = value.as_i64()
        && (schema
            .get("minimum")
            .and_then(Value::as_i64)
            .is_some_and(|m| n < m)
            || schema
                .get("maximum")
                .and_then(Value::as_i64)
                .is_some_and(|m| n > m))
    {
        return fail("out of range".into());
    }
    if let Some(items) = value.as_array() {
        if schema
            .get("maxItems")
            .and_then(Value::as_u64)
            .is_some_and(|m| items.len() as u64 > m)
            || schema
                .get("minItems")
                .and_then(Value::as_u64)
                .is_some_and(|m| (items.len() as u64) < m)
        {
            return fail("item count out of range".into());
        }
        if let Some(item) = schema.get("items") {
            for (i, v) in items.iter().enumerate() {
                check(contract, item, v, &format!("{at}[{i}]"))?;
            }
        }
    }
    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(name) {
                    return fail(format!("missing {name}"));
                }
            }
        }
        for (name, member) in object {
            match properties.and_then(|p| p.get(name)) {
                Some(s) => check(contract, s, member, &format!("{at}.{name}"))?,
                None if schema.get("additionalProperties") == Some(&Value::Bool(false)) => {
                    return fail(format!("unexpected member {name}"));
                }
                None => {}
            }
        }
    }
    for every in schema
        .get("allOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        check(contract, every, value, at)?;
    }
    if let Some(options) = schema.get("anyOf").and_then(Value::as_array)
        && !options
            .iter()
            .any(|o| check(contract, o, value, at).is_ok())
    {
        return fail("matches no anyOf".into());
    }
    if let Some(options) = schema.get("oneOf").and_then(Value::as_array)
        && options
            .iter()
            .filter(|o| check(contract, o, value, at).is_ok())
            .count()
            != 1
    {
        return fail("matches not exactly one oneOf".into());
    }
    if let Some(condition) = schema.get("if") {
        let branch = if check(contract, condition, value, at).is_ok() {
            schema.get("then")
        } else {
            schema.get("else")
        };
        if let Some(branch) = branch {
            check(contract, branch, value, at)?;
        }
    }
    Ok(())
}

pub(crate) fn conforms(schema: &str, body: &Value) {
    let contract = contract();
    if let Err(why) = check(
        &contract,
        &json!({"$ref": format!("#/components/schemas/{schema}")}),
        body,
        schema,
    ) {
        panic!("{why}");
    }
}
