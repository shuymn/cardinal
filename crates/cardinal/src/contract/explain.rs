use serde::{Deserialize, Serialize};

use super::ExplainSchema;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExplainKind {
    Job,
    Plan,
    Inbox,
    Command,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainSubject {
    pub kind: ExplainKind,
    pub id: String,
}

/// Where a decision came from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExplainSource {
    Config {
        file: String,
        key: String,
        value: serde_json::Value,
    },
    Rule {
        rule: String,
        pattern: String,
        normalized_argv: Vec<String>,
    },
    Observation {
        command: Vec<String>,
        exit: Option<i32>,
        result: String,
    },
    Record {
        record: serde_json::Map<String, serde_json::Value>,
    },
}

/// `cardinal.explain.v1`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Explain {
    pub schema: ExplainSchema,
    pub subject: ExplainSubject,
    pub summary: String,
    pub sources: Vec<ExplainSource>,
    /// For `command`: the rewrite the hook would produce.
    pub rewritten: Option<String>,
    pub outcome: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::ExplainSource;

    #[test]
    fn sources_are_tagged_by_kind() {
        let source = ExplainSource::Observation {
            command: vec!["git".into(), "status".into()],
            exit: None,
            result: "timeout".into(),
        };
        assert_eq!(
            serde_json::to_value(source).unwrap(),
            serde_json::json!({
                "kind": "observation", "command": ["git", "status"], "exit": null, "result": "timeout"
            })
        );
    }
}
