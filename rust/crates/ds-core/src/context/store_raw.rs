//! Raw JSON boundary for Python Store.add. Conversion happens where Python does it,
//! including a predecessor write before a malformed new body fails at `.strip()`.
use super::*;
use crate::api::{python_value_repr, python_value_string, truthy, Args};
use crate::error::DunkError;
use serde_json::Value;

pub(crate) fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.to_string().contains(['.', 'e', 'E']) => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

impl Store {
    pub fn find_raw(&self, value: &Value) -> Option<Entry> {
        value.as_str().and_then(|s| self.find(s))
    }

    pub fn resolve_raw(&self, value: &Value) -> Result<Option<Entry>, ContextError> {
        match value.as_str() {
            Some(value) => self.resolve(value),
            None => Ok(None),
        }
    }

    pub fn add_raw(
        &self,
        args: &Args,
        by: Option<&str>,
        ledger: &mut Ledger,
    ) -> Result<Added, DunkError> {
        let get = |key: &str| args.get(key).unwrap_or(&Value::Null);
        let cls = get("cls");
        let authority = get("authority");
        let type_ = get("type");
        if !cls.as_str().is_some_and(|v| CLASSES.contains(&v)) {
            return Err(DunkError::Refused(format!(
                "class must be one of {}",
                tuple_repr(&CLASSES)
            )));
        }
        if !authority.as_str().is_some_and(|v| AUTHORITIES.contains(&v)) {
            return Err(DunkError::Refused(format!(
                "authority must be one of {}",
                tuple_repr(&AUTHORITIES)
            )));
        }
        if !type_.as_str().is_some_and(|v| TYPES.contains(&v)) {
            return Err(DunkError::Refused(format!(
                "type must be one of {}; got {}",
                tuple_repr(&TYPES),
                python_value_repr(type_)
            )));
        }
        if cls == "ground-truth" && authority != "owner" {
            return Err(DunkError::Refused("a ground truth has authority owner; an agent proposes a fact and the owner promotes it".into()));
        }
        self.ensure().map_err(|e| DunkError::Refused(e.0))?;
        let stated = if truthy(get("stated")) {
            get("stated").clone()
        } else {
            Value::String(today())
        };
        let mut slug_text = format!(
            "{}-{}",
            python_value_string(&stated),
            slug(&python_value_string(get("subject")))
        );
        let class = cls.as_str().expect("validated class");
        let folder = self.root.join(class);
        let mut path = folder.join(format!("{slug_text}.md"));
        let mut supersedes = get("supersedes").clone();
        if path.exists() {
            if !truthy(&supersedes) {
                return Err(DunkError::Refused(format!(
                    "entry {slug_text} already exists"
                )));
            }
            let mut counter = 2;
            while folder.join(format!("{slug_text}-{counter}.md")).exists() {
                counter += 1;
            }
            slug_text = format!("{slug_text}-{counter}");
            path = folder.join(format!("{slug_text}.md"));
        }
        if truthy(&supersedes) {
            let old = self
                .resolve_raw(&supersedes)
                .map_err(|e| DunkError::Refused(e.0))?
                .ok_or_else(|| {
                    DunkError::Refused(format!(
                        "supersede target {} not found",
                        python_value_string(&supersedes)
                    ))
                })?;
            supersedes = Value::String(old.slug.clone());
            let mut meta = old.meta.clone();
            meta.insert("superseded_by".into(), Some(slug_text.clone()));
            let text = render(&meta, &old.body);
            fs::write(&old.path, &text)?;
            let text = read_written_text(&old.path)?;
            ledger
                .record(
                    &self.root,
                    &old.slug,
                    &text,
                    old.get("class"),
                    old.get("authority"),
                    by,
                )
                .map_err(crate::db::map_sqlite)?;
        }
        let mut meta = Meta::new();
        for (key, value) in [
            ("class", cls.clone()),
            ("subject", get("subject").clone()),
            ("type", type_.clone()),
            ("scope", Value::String(self.scope.into())),
            ("authority", authority.clone()),
            ("stated", stated),
            ("source", get("source").clone()),
            (
                "recheck_after",
                if get("recheck_after").is_null() && class == "provisional" {
                    Value::String("7d".into())
                } else {
                    get("recheck_after").clone()
                },
            ),
            ("supersedes", supersedes),
        ] {
            meta.insert(
                key.into(),
                if value.is_null() {
                    None
                } else {
                    Some(python_value_string(&value))
                },
            );
        }
        let body = get("body").as_str().ok_or_else(|| {
            DunkError::Refused(format!(
                "'{}' object has no attribute 'strip'",
                type_name(get("body"))
            ))
        })?;
        let text = render(&meta, body);
        fs::write(&path, &text)?;
        let text = read_written_text(&path)?;
        ledger
            .record(
                &self.root,
                &slug_text,
                &text,
                Some(class),
                authority.as_str(),
                by,
            )
            .map_err(crate::db::map_sqlite)?;
        Ok(Added {
            slug: slug_text,
            path,
            cls: class.into(),
        })
    }
}

/// Path.read_text() reopens the written file, decodes strictly, and applies
/// universal newlines. Keep this fallible read after the write and before SQL;
/// hashing the pre-write rendering loses both the bytes read and this boundary.
fn read_written_text(path: &Path) -> Result<String, DunkError> {
    Ok(crate::context::read_text(path)?)
}

#[cfg(test)]
mod postwrite_tests {
    use super::*;

    #[test]
    fn postwrite_read_is_fallible_strict_and_only_normalizes_cr_newlines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("written.md");
        fs::write(
            &path,
            "first\r\nsecond\rthird\nfourth\u{85}\u{2028}\u{2029} café",
        )
        .unwrap();
        assert_eq!(
            read_written_text(&path).unwrap(),
            "first\nsecond\nthird\nfourth\u{85}\u{2028}\u{2029} café"
        );
        fs::write(&path, [0xff, b'\r', b'\n']).unwrap();
        assert!(read_written_text(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), [0xff, b'\r', b'\n']);
        fs::remove_file(&path).unwrap();
        assert!(read_written_text(&path).is_err());
    }
}
