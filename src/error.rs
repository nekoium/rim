use serde::Serialize;
use std::{fmt, io};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Usage,
    State,
    Conflict,
    Confirmation,
    Git,
    Corruption,
    Io,
}

#[derive(Debug)]
pub struct Error {
    pub category: Category,
    pub message: String,
}

impl Error {
    pub fn new(category: Category, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
        }
    }
    pub fn state(message: impl Into<String>) -> Self {
        Self::new(Category::State, message)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(Category::Conflict, message)
    }
    pub fn corruption(message: impl Into<String>) -> Self {
        Self::new(Category::Corruption, message)
    }
    pub fn code(&self) -> i32 {
        match self.category {
            Category::Usage => 2,
            Category::State => 3,
            Category::Conflict => 4,
            Category::Confirmation => 5,
            Category::Git => 6,
            Category::Corruption => 7,
            Category::Io => 8,
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::new(Category::Io, e.to_string())
    }
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::new(Category::Corruption, format!("metadata error: {e}"))
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::new(Category::Corruption, format!("data error: {e}"))
    }
}
