//! Document filter matching for `find({ ... })`.

use grumpydb::Value;

/// Returns true if the document matches the filter.
///
/// Each key in the filter is checked against the document:
/// - `{ age: 30 }` → doc["age"] == 30
/// - `{ "address.city": "Paris" }` → doc["address"]["city"] == "Paris"
/// - `{}` → matches everything
pub fn matches_filter(doc: &Value, filter: &Value) -> bool {
    let Some(filter_obj) = filter.as_object() else {
        return false;
    };

    if filter_obj.is_empty() {
        return true;
    }

    let Value::Object(_) = doc else {
        return false;
    };

    for (field_path, expected) in filter_obj {
        let actual = extract_nested(doc, field_path);
        match actual {
            Some(val) if val == expected => {}
            _ => return false,
        }
    }

    true
}

/// Extracts a nested field using dot-notation.
fn extract_nested<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for part in path.split('.') {
        match current {
            Value::Object(obj) => {
                current = obj.get(part)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn make_doc() -> Value {
        Value::Object(BTreeMap::from([
            ("name".into(), Value::String("Alice".into())),
            ("age".into(), Value::Integer(30)),
            (
                "address".into(),
                Value::Object(BTreeMap::from([(
                    "city".into(),
                    Value::String("Paris".into()),
                )])),
            ),
        ]))
    }

    #[test]
    fn test_empty_filter_matches_all() {
        let doc = make_doc();
        let filter = Value::Object(BTreeMap::new());
        assert!(matches_filter(&doc, &filter));
    }

    #[test]
    fn test_simple_match() {
        let doc = make_doc();
        let filter = Value::Object(BTreeMap::from([("age".into(), Value::Integer(30))]));
        assert!(matches_filter(&doc, &filter));
    }

    #[test]
    fn test_simple_mismatch() {
        let doc = make_doc();
        let filter = Value::Object(BTreeMap::from([("age".into(), Value::Integer(25))]));
        assert!(!matches_filter(&doc, &filter));
    }

    #[test]
    fn test_nested_match() {
        let doc = make_doc();
        let filter = Value::Object(BTreeMap::from([(
            "address.city".into(),
            Value::String("Paris".into()),
        )]));
        assert!(matches_filter(&doc, &filter));
    }

    #[test]
    fn test_missing_field() {
        let doc = make_doc();
        let filter = Value::Object(BTreeMap::from([(
            "email".into(),
            Value::String("a@b.com".into()),
        )]));
        assert!(!matches_filter(&doc, &filter));
    }

    #[test]
    fn test_multi_field_filter() {
        let doc = make_doc();
        let filter = Value::Object(BTreeMap::from([
            ("name".into(), Value::String("Alice".into())),
            ("age".into(), Value::Integer(30)),
        ]));
        assert!(matches_filter(&doc, &filter));
    }
}
