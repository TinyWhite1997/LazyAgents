//! Minimal `{{ key }}` substitution for service unit templates.
//!
//! We intentionally don't pull in `tera` / `handlebars` / `askama` —
//! the templates ship with the binary, the placeholders are a fixed
//! finite list, and the renderer needs to be auditable end-to-end so a
//! reviewer can confirm no shell variable leaks into the launchd plist
//! (A3 hard rule).
//!
//! Behaviour:
//!   * Replaces every `{{ key }}` (whitespace inside the braces is
//!     ignored) with the matching value from the supplied lookup.
//!   * An unknown key is an error — there is no silent "" fallback,
//!     because a missing exec path or config path producing an empty
//!     unit file is a worse failure mode than refusing to install.

use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    #[error("template references unknown placeholder `{{{{ {0} }}}}`")]
    UnknownPlaceholder(String),
    #[error("template has unterminated `{{{{` block")]
    UnterminatedBlock,
}

/// Render a template by substituting `{{ key }}` markers with values
/// from `vars`. Keys in `vars` are matched after trimming whitespace
/// from inside the braces.
pub fn render_template(
    template: &str,
    vars: &BTreeMap<&str, &str>,
) -> Result<String, TemplateError> {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // Find the closing `}}`.
            let rest = &template[i + 2..];
            let end = rest.find("}}").ok_or(TemplateError::UnterminatedBlock)?;
            let key = rest[..end].trim();
            let value = vars
                .get(key)
                .ok_or_else(|| TemplateError::UnknownPlaceholder(key.to_string()))?;
            out.push_str(value);
            i += 2 + end + 2;
        } else {
            // UTF-8 safe push of the next codepoint.
            let ch = template[i..].chars().next().expect("non-empty");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_known_keys() {
        let mut vars = BTreeMap::new();
        vars.insert("a", "1");
        vars.insert("b", "two");
        assert_eq!(
            render_template("a={{ a }}; b={{b}}", &vars).unwrap(),
            "a=1; b=two"
        );
    }

    #[test]
    fn rejects_unknown_placeholder() {
        let vars = BTreeMap::new();
        let err = render_template("hi {{ name }}", &vars).unwrap_err();
        match err {
            TemplateError::UnknownPlaceholder(k) => assert_eq!(k, "name"),
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn rejects_unterminated() {
        let mut vars = BTreeMap::new();
        vars.insert("x", "y");
        let err = render_template("oops {{ x", &vars).unwrap_err();
        assert!(matches!(err, TemplateError::UnterminatedBlock));
    }

    #[test]
    fn preserves_braces_outside_double() {
        let mut vars = BTreeMap::new();
        vars.insert("x", "1");
        assert_eq!(
            render_template("{ literal } {{ x }} { again }", &vars).unwrap(),
            "{ literal } 1 { again }"
        );
    }
}
