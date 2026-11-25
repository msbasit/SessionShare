use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Both sides are identical.
    Identical,
    /// Local has all remote lines plus new ones (fast-forward push).
    LocalAhead(String),
    /// Remote has all local lines plus new ones (fast-forward pull).
    RemoteAhead(String),
    /// Both sides have unique lines; merged cleanly in chronological order.
    Merged {
        content: String,
        local_only_count: usize,
        remote_only_count: usize,
    },
}

#[derive(Debug, Clone)]
struct ParsedLine {
    key: String,
    timestamp: Option<String>,
    raw: String,
    order: usize,
}

fn line_key(raw: &str) -> (String, Option<String>) {
    if let Ok(val) = serde_json::from_str::<Value>(raw) {
        let ts = val
            .get("timestamp")
            .and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            });

        if let Some(uuid) = val.get("uuid").and_then(|v| v.as_str()) {
            return (format!("uuid:{}", uuid), ts);
        }
        // history.jsonl entries don't always have uuid; use (timestamp, sessionId, display)
        if let (Some(t), Some(display)) = (
            ts.as_deref(),
            val.get("display").and_then(|v| v.as_str()),
        ) {
            let sid = val.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
            let mut hasher = Sha256::new();
            hasher.update(format!("{}|{}|{}", t, sid, display).as_bytes());
            return (format!("hist:{}", hex::encode(hasher.finalize())), ts);
        }
    }

    let mut hasher = Sha256::new();
    hasher.update(raw.trim().as_bytes());
    (format!("raw:{}", hex::encode(hasher.finalize())), None)
}

fn parse_lines(content: &str) -> Vec<ParsedLine> {
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(idx, line)| {
            let (key, timestamp) = line_key(line);
            ParsedLine {
                key,
                timestamp,
                raw: line.to_string(),
                order: idx,
            }
        })
        .collect()
}

/// Merges two normalized `.jsonl` strings (both in portable form) without losing any lines.
pub fn merge_jsonl(local_portable: &str, remote_portable: &str) -> MergeOutcome {
    let local_lines = parse_lines(local_portable);
    let remote_lines = parse_lines(remote_portable);

    let local_keys: HashSet<&str> = local_lines.iter().map(|l| l.key.as_str()).collect();
    let remote_keys: HashSet<&str> = remote_lines.iter().map(|l| l.key.as_str()).collect();

    let local_only_count = local_lines
        .iter()
        .filter(|l| !remote_keys.contains(l.key.as_str()))
        .count();
    let remote_only_count = remote_lines
        .iter()
        .filter(|l| !local_keys.contains(l.key.as_str()))
        .count();

    if local_only_count == 0 && remote_only_count == 0 {
        return MergeOutcome::Identical;
    }
    if remote_only_count == 0 {
        return MergeOutcome::LocalAhead(join_lines(&local_lines));
    }
    if local_only_count == 0 {
        return MergeOutcome::RemoteAhead(join_lines(&remote_lines));
    }

    // Both sides have unique lines: deduplicate and combine, sorting by timestamp when present.
    let mut seen = HashSet::new();
    let mut combined: Vec<(ParsedLine, u8)> = Vec::new();
    for l in local_lines {
        if seen.insert(l.key.clone()) {
            combined.push((l, 0));
        }
    }
    for r in remote_lines {
        if seen.insert(r.key.clone()) {
            combined.push((r, 1));
        }
    }

    let all_have_ts = combined.iter().all(|(l, _)| l.timestamp.is_some());
    if all_have_ts {
        combined.sort_by(|(a, side_a), (b, side_b)| {
            a.timestamp
                .cmp(&b.timestamp)
                .then_with(|| side_a.cmp(side_b))
                .then_with(|| a.order.cmp(&b.order))
        });
    }

    let merged_lines: Vec<ParsedLine> = combined.into_iter().map(|(l, _)| l).collect();
    MergeOutcome::Merged {
        content: join_lines(&merged_lines),
        local_only_count,
        remote_only_count,
    }
}

fn join_lines(lines: &[ParsedLine]) -> String {
    let mut out = String::new();
    for l in lines {
        out.push_str(&l.raw);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_forwards_when_one_side_is_subset() {
        let a = r#"{"uuid":"1","timestamp":"2026-01-01T00:00:00Z","text":"a"}
{"uuid":"2","timestamp":"2026-01-01T00:01:00Z","text":"b"}
"#;
        let b = r#"{"uuid":"1","timestamp":"2026-01-01T00:00:00Z","text":"a"}
"#;
        assert!(matches!(merge_jsonl(a, b), MergeOutcome::LocalAhead(_)));
        assert!(matches!(merge_jsonl(b, a), MergeOutcome::RemoteAhead(_)));
        assert_eq!(merge_jsonl(a, a), MergeOutcome::Identical);
    }

    #[test]
    fn merges_divergent_appends_chronologically() {
        let local = r#"{"uuid":"1","timestamp":"2026-01-01T00:00:00Z","text":"base"}
{"uuid":"2","timestamp":"2026-01-01T00:02:00Z","text":"local-later"}
"#;
        let remote = r#"{"uuid":"1","timestamp":"2026-01-01T00:00:00Z","text":"base"}
{"uuid":"3","timestamp":"2026-01-01T00:01:00Z","text":"remote-earlier"}
"#;
        match merge_jsonl(local, remote) {
            MergeOutcome::Merged {
                content,
                local_only_count,
                remote_only_count,
            } => {
                assert_eq!(local_only_count, 1);
                assert_eq!(remote_only_count, 1);
                let lines: Vec<&str> = content.lines().collect();
                assert_eq!(lines.len(), 3);
                assert!(lines[1].contains("remote-earlier"));
                assert!(lines[2].contains("local-later"));
            }
            other => panic!("Expected Merged, got {:?}", other),
        }
    }
}
