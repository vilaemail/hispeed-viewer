use sha2::{Digest, Sha256};
use std::path::Path;

/// Verify that a file's SHA-256 hash matches the expected value.
pub fn verify_sha256(file_path: &Path, expected_hex: &str) -> Result<bool, String> {
    let hash = compute_sha256(file_path)?;
    Ok(hash == expected_hex.to_lowercase())
}

/// Compute SHA-256 hash of a file, returning lowercase hex string.
pub fn compute_sha256(file_path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(file_path)
        .map_err(|e| format!("Cannot open {}: {e}", file_path.display()))?;

    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .map_err(|e| format!("Read error on {}: {e}", file_path.display()))?;

    let hash = hasher.finalize();
    Ok(format!("{:x}", hash))
}
