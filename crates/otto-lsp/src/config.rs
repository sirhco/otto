//! Resolver from the raw `config.lsp` JSON value (kept as `Option<Value>` in
//! `otto-config` to avoid destabilizing config deep-merge) into the typed
//! `LspConfigResolved` the service consumes. Mirrors opencode's
//! `config/lsp.ts:149-189` merge.

use crate::service::{LspConfigResolved, ServerOverride};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

pub fn resolve(lsp: Option<&Value>) -> LspConfigResolved {
    match lsp {
        Some(Value::Bool(false)) => LspConfigResolved {
            enabled: false,
            overrides: HashMap::new(),
            disabled: HashSet::new(),
        },
        None | Some(Value::Bool(true)) => LspConfigResolved::enabled_default(),
        Some(Value::Object(map)) => {
            let mut overrides = HashMap::new();
            let mut disabled = HashSet::new();
            for (name, item) in map {
                if item.get("disabled").and_then(Value::as_bool) == Some(true) {
                    disabled.insert(name.clone());
                    continue;
                }
                let command: Vec<String> = item
                    .get("command")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                if command.is_empty() {
                    continue; // invalid override without a command
                }
                let extensions = item.get("extensions").and_then(Value::as_array).map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect::<Vec<_>>()
                });
                let env = item
                    .get("env")
                    .and_then(Value::as_object)
                    .map(|o| {
                        o.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect()
                    })
                    .unwrap_or_default();
                let initialization = item.get("initialization").cloned();
                overrides.insert(
                    name.clone(),
                    ServerOverride {
                        command,
                        extensions,
                        env,
                        initialization,
                    },
                );
            }
            LspConfigResolved {
                enabled: true,
                overrides,
                disabled,
            }
        }
        _ => LspConfigResolved::enabled_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn false_disables_all() {
        let r = resolve(Some(&json!(false)));
        assert!(!r.enabled);
    }

    #[test]
    fn absent_enables_builtins() {
        let r = resolve(None);
        assert!(r.enabled);
        assert!(r.overrides.is_empty());
    }

    #[test]
    fn object_with_disabled_and_override() {
        let cfg = json!({
            "gopls": { "disabled": true },
            "myls": { "command": ["my-ls", "--stdio"], "extensions": [".foo"] }
        });
        let r = resolve(Some(&cfg));
        assert!(r.enabled);
        assert!(r.disabled.contains("gopls"));
        let o = r.overrides.get("myls").unwrap();
        assert_eq!(o.command, vec!["my-ls", "--stdio"]);
        assert_eq!(o.extensions.as_deref(), Some(&[".foo".to_string()][..]));
    }
}
