//! Helpers for `settings.json`: keeping machine-local keys (secrets) off the synced drive and
//! detecting changes to keys that make Claude Code run commands or widen its permissions.

use serde_json::{Map, Value};

/// Top-level `settings.json` keys that execute commands or grant permissions.
/// Incoming changes to these require explicit approval.
pub const COMMAND_KEYS: [&str; 7] = [
    "hooks",
    "statusLine",
    "apiKeyHelper",
    "awsAuthRefresh",
    "awsCredentialExport",
    "otelHeadersHelper",
    "permissions",
];

fn parse_object(content: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(content).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// Removes the given top-level keys. Non-object / invalid JSON is returned unchanged.
pub fn strip_keys(content: &str, keys: &[String]) -> String {
    let Some(mut map) = parse_object(content) else {
        return content.to_string();
    };
    for k in keys {
        map.remove(k);
    }
    serde_json::to_string_pretty(&Value::Object(map)).unwrap_or_else(|_| content.to_string())
}

/// Returns true if any of the given top-level keys is present.
pub fn has_any_key(content: &str, keys: &[String]) -> bool {
    parse_object(content)
        .map(|m| keys.iter().any(|k| m.contains_key(k)))
        .unwrap_or(false)
}

/// Takes incoming (remote) settings and restores this PC's own values for the excluded keys,
/// so pulling never wipes or overwrites local secrets.
pub fn reinject_keys(incoming: &str, local: Option<&str>, keys: &[String]) -> String {
    let Some(mut map) = parse_object(incoming) else {
        return incoming.to_string();
    };
    let local_map = local.and_then(parse_object);
    for k in keys {
        map.remove(k);
        if let Some(v) = local_map.as_ref().and_then(|m| m.get(k)) {
            map.insert(k.clone(), v.clone());
        }
    }
    serde_json::to_string_pretty(&Value::Object(map)).unwrap_or_else(|_| incoming.to_string())
}

/// Stable serialization of all command-bearing keys (empty object if none / invalid JSON).
pub fn command_signature(content: Option<&str>) -> String {
    let mut out = Map::new();
    if let Some(map) = content.and_then(parse_object) {
        for k in COMMAND_KEYS {
            if let Some(v) = map.get(k) {
                out.insert(k.to_string(), v.clone());
            }
        }
    }
    serde_json::to_string_pretty(&Value::Object(out)).unwrap_or_default()
}

fn merge_value(
    base: Option<&Value>,
    local: Option<&Value>,
    remote: Option<&Value>,
) -> Option<Option<Value>> {
    if local == remote {
        return Some(local.cloned());
    }
    if local == base {
        return Some(remote.cloned());
    }
    if remote == base {
        return Some(local.cloned());
    }
    let (Some(Value::Object(l)), Some(Value::Object(r))) = (local, remote) else {
        return None;
    };
    let b = match base {
        Some(Value::Object(b)) => Some(b),
        _ => None,
    };
    let mut keys: Vec<&String> = l.keys().collect();
    keys.extend(r.keys().filter(|k| !l.contains_key(*k)));
    let mut out = Map::new();
    for k in keys {
        if let Some(v) = merge_value(b.and_then(|b| b.get(k)), l.get(k), r.get(k))? {
            out.insert(k.clone(), v);
        }
    }
    Some(Some(Value::Object(out)))
}

/// Key-by-key 3-way merge of JSON documents. Returns `None` if the same key was changed
/// differently on both sides (or any input is not valid JSON).
pub fn json_merge3(base: &str, local: &str, remote: &str) -> Option<String> {
    let parse = |s: &str| serde_json::from_str::<Value>(s).ok();
    let (b, l, r) = (parse(base)?, parse(local)?, parse(remote)?);
    let merged = merge_value(Some(&b), Some(&l), Some(&r))??;
    serde_json::to_string_pretty(&merged).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excluded_keys_stay_local_on_pull() {
        let keys = vec!["env".to_string()];
        let local = r#"{"env":{"API_KEY":"secret"},"model":"a"}"#;
        let incoming = r#"{"env":{"API_KEY":"other"},"model":"b"}"#;
        assert!(!strip_keys(local, &keys).contains("secret"));
        let merged = reinject_keys(incoming, Some(local), &keys);
        assert!(merged.contains("secret") && !merged.contains("other") && merged.contains("\"b\""));
        assert!(!reinject_keys(incoming, None, &keys).contains("env"));
    }

    #[test]
    fn json_merge_combines_independent_key_changes() {
        let base = r#"{"model":"a","permissions":{"allow":["x"]},"theme":"dark"}"#;
        let local = r#"{"model":"b","permissions":{"allow":["x"]},"theme":"dark"}"#;
        let remote = r#"{"model":"a","permissions":{"allow":["x"],"deny":["y"]}}"#;
        let merged: Value = serde_json::from_str(&json_merge3(base, local, remote).unwrap()).unwrap();
        assert_eq!(merged["model"], "b");
        assert_eq!(merged["permissions"]["deny"][0], "y");
        assert!(merged.get("theme").is_none());

        let clash = r#"{"model":"c","permissions":{"allow":["x"]},"theme":"dark"}"#;
        assert!(json_merge3(base, local, clash).is_none());
    }

    #[test]
    fn signature_only_tracks_command_keys() {
        let a = r#"{"model":"a","hooks":{"Stop":[]}}"#;
        let b = r#"{"model":"b","hooks":{"Stop":[]}}"#;
        let c = r#"{"model":"a","hooks":{"Stop":[{"command":"evil"}]}}"#;
        assert_eq!(command_signature(Some(a)), command_signature(Some(b)));
        assert_ne!(command_signature(Some(a)), command_signature(Some(c)));
    }
}
