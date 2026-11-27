//! 统一错误类型：稳定错误码 + 中文消息 + 可选建议。

use serde_json::Value;

/// 所有对外可见的错误码。新增错误码必须加进来：
/// `tests::every_error_code_is_documented_in_the_skill` 会检查 SKILL.md 的错误表。
pub const CODES: &[&str] = &[
    "ACTION_FAILED",
    "BAD_ARGS",
    "BAD_REF",
    "BAD_WINDOW_ID",
    "BG_DELIVER_FAILED",
    "CLIPBOARD_FAILED",
    "CLIPBOARD_TOO_LARGE",
    "CLOSE_NOT_CONFIRMED",
    "DISABLED",
    "ELEVATE_FAILED",
    "ELEVATE_TIMEOUT",
    "ELEVATED_COMMAND_FAILED",
    "ELEVATION_MISMATCH",
    "LAUNCH_FAILED",
    "NEED_ADMIN",
    "NO_FOREGROUND",
    "NO_PATTERN",
    "NO_TEXT",
    "PASTE_FAILED",
    "POINT_OUTSIDE",
    "POINT_UNKNOWN",
    "RUN_FAILED",
    "SELFTEST_FAILED",
    "SHOT_EMPTY",
    "SHOT_FAILED",
    "SNAPSHOT_NOT_FOUND",
    "STORE_FAILED",
    "TARGET_PROTECTED",
    "TOOL_TOKEN_UNREADABLE",
    "UIA_FAILED",
    "UIA_INIT",
    "UIA_UNREADABLE",
    "WAIT_TIMEOUT",
    "WATCHDOG_TIMEOUT",
    "WINDOW_GONE",
    "WINDOW_HIDDEN",
    "WINDOW_HUNG",
    "WINDOW_MINIMIZED",
    "WINDOW_MOVE_FAILED",
    "WINDOW_REUSED",
];

#[derive(Debug, Clone)]
pub struct DeskError {
    pub code: &'static str,
    pub message: String,
    pub suggestion: Option<String>,
    pub details: Option<Value>,
    pub retry_safe: bool,
}

impl DeskError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        debug_assert!(CODES.contains(&code), "未登记的错误码：{code}");
        Self {
            code,
            message: message.into(),
            suggestion: None,
            details: None,
            retry_safe: true,
        }
    }

    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// 动作可能已经落地，重试不安全。
    pub fn delivered(mut self) -> Self {
        self.retry_safe = false;
        self
    }

    pub fn to_json(&self) -> Value {
        let mut err = serde_json::json!({
            "code": self.code,
            "message": self.message,
            "retry": if self.retry_safe { "safe" } else { "unsafe" },
        });
        if let Some(s) = &self.suggestion {
            err["suggestion"] = Value::String(s.clone());
        }
        if let Some(d) = &self.details {
            err["details"] = d.clone();
        }
        err
    }
}

impl std::fmt::Display for DeskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DeskError {}

pub type Result<T> = std::result::Result<T, DeskError>;
