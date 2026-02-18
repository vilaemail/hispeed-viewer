use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceStatus {
    Available,
    Unavailable,
    Checking,
}

#[derive(Debug, Clone)]
pub struct RemoteFileEntry {
    pub json_name: String,
    pub size: i64,
}

#[derive(Debug, Clone)]
pub struct FetchResult {
    pub success: bool,
    pub error: Option<String>,
}

impl FetchResult {
    pub fn ok() -> Self {
        Self {
            success: true,
            error: None,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            success: false,
            error: Some(msg.into()),
        }
    }
}

pub trait RemoteSource: Send {
    fn status(&self) -> SourceStatus;
    fn refresh_status(&mut self);
    fn list_files(&mut self) -> Vec<RemoteFileEntry>;
    fn fetch_file(&self, remote_name: &str, dest: &Path) -> FetchResult;
    fn display_name(&self) -> &str;
}
