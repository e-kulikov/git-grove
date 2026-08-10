use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitClass {
    Ok,
    Failure,
    NeedsDecision,
    Usage,
}

impl ExitClass {
    pub fn code(&self) -> u8 {
        match self {
            ExitClass::Ok => 0,
            ExitClass::Failure => 1,
            ExitClass::NeedsDecision => 2,
            ExitClass::Usage => 64,
        }
    }
}

#[derive(Debug)]
pub struct GroveError {
    pub class: ExitClass,
    pub message: String,
    pub detail: Option<String>,
}

impl GroveError {
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            class: ExitClass::Usage,
            message: message.into(),
            detail: None,
        }
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            class: ExitClass::Failure,
            message: message.into(),
            detail: None,
        }
    }

    pub fn needs_decision(message: impl Into<String>) -> Self {
        Self {
            class: ExitClass::NeedsDecision,
            message: message.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

impl fmt::Display for GroveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(detail) = &self.detail {
            write!(f, "\n  {detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for GroveError {}

pub type Result<T> = std::result::Result<T, GroveError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_classes_to_documented_codes() {
        assert_eq!(ExitClass::Ok.code(), 0);
        assert_eq!(ExitClass::Failure.code(), 1);
        assert_eq!(ExitClass::NeedsDecision.code(), 2);
        assert_eq!(ExitClass::Usage.code(), 64);
    }

    #[test]
    fn carries_message_and_detail() {
        let err = GroveError::usage("not a grove").with_detail("run `git grove clone` first");
        assert_eq!(err.class, ExitClass::Usage);
        assert_eq!(err.message, "not a grove");
        assert_eq!(err.detail.as_deref(), Some("run `git grove clone` first"));
    }
}
