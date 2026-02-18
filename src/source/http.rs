use super::traits::*;
use std::path::Path;

pub struct HttpSource {
    base_url: String,
    status: SourceStatus,
}

impl HttpSource {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            status: SourceStatus::Checking,
        }
    }
}

impl RemoteSource for HttpSource {
    fn status(&self) -> SourceStatus {
        self.status
    }

    fn refresh_status(&mut self) {
        let client = match reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(_) => {
                self.status = SourceStatus::Unavailable;
                return;
            }
        };

        match client.get(&self.base_url).send() {
            Ok(resp) if resp.status().is_success() => {
                self.status = SourceStatus::Available;
            }
            _ => {
                self.status = SourceStatus::Unavailable;
            }
        }
    }

    fn list_files(&mut self) -> Vec<RemoteFileEntry> {
        if self.status != SourceStatus::Available {
            return Vec::new();
        }

        let client = match reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };

        // Try to get file listing from the HTTP server.
        // The hispeed-trigger-cam server provides a JSON listing.
        let url = format!("{}/files", self.base_url);
        match client.get(&url).send() {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(text) = resp.text() {
                    // Try parsing as JSON array of file entries
                    if let Ok(entries) = serde_json::from_str::<Vec<HttpFileEntry>>(&text) {
                        return entries
                            .into_iter()
                            .filter(|e| e.name.ends_with(".json"))
                            .map(|e| RemoteFileEntry {
                                json_name: e.name,
                                size: e.size.unwrap_or(0),
                            })
                            .collect();
                    }
                }
                Vec::new()
            }
            _ => {
                self.status = SourceStatus::Unavailable;
                Vec::new()
            }
        }
    }

    fn fetch_file(&self, remote_name: &str, dest: &Path) -> FetchResult {
        let client = match reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
        {
            Ok(c) => c,
            Err(e) => return FetchResult::err(format!("HTTP client error: {e}")),
        };

        let url = format!("{}/files/{}", self.base_url, remote_name);
        match client.get(&url).send() {
            Ok(resp) if resp.status().is_success() => {
                match resp.bytes() {
                    Ok(bytes) => {
                        if let Err(e) = std::fs::write(dest, &bytes) {
                            return FetchResult::err(format!("Write failed: {e}"));
                        }
                        FetchResult::ok()
                    }
                    Err(e) => FetchResult::err(format!("Download failed: {e}")),
                }
            }
            Ok(resp) => FetchResult::err(format!("HTTP {}", resp.status())),
            Err(e) => FetchResult::err(format!("Connection failed: {e}")),
        }
    }

    fn display_name(&self) -> &str {
        "HTTP Server"
    }
}

#[derive(serde::Deserialize)]
struct HttpFileEntry {
    name: String,
    size: Option<i64>,
}
