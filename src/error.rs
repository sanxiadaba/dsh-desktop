//! 统一错误类型：稳定错误码 + 中文消息 + 可选建议。

use serde_json::Value;

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

pub fn last_os_error(code: &'static str, what: &str) -> DeskError {
    let e = std::io::Error::last_os_error();
    DeskError::new(code, format!("{what}失败：{e}"))
}
