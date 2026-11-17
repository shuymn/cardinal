//! The classifier unit seam (`docs/architecture.md` section 7.6). The built-in classifier
//! speaks these types in process; external units (phase 1+) speak them as JSON.

use serde::{Deserialize, Serialize};

use super::{ClassifyRequestSchema, ClassifyResponseSchema, Resources};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifyRepo {
    pub common_dir: String,
    pub main_worktree: String,
    pub remote: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifyTool {
    /// `"Bash"`
    pub name: String,
    pub timeout_ms: u64,
    pub background: bool,
}

/// `cardinal.classify-request.v1`: one stage to classify.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifyRequest {
    pub schema: ClassifyRequestSchema,
    pub command: String,
    pub stage: String,
    /// The normalized argv (`docs/architecture.md` section 7.3).
    pub argv: Vec<String>,
    pub cwd: String,
    pub repo: Option<ClassifyRepo>,
    pub tool: ClassifyTool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassifyDecision {
    Wrap,
    Pass,
}

/// `cardinal.classify-response.v1`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifyResponse {
    pub schema: ClassifyResponseSchema,
    pub decision: ClassifyDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    pub reason: String,
}
