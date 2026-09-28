//! Secure secret storage (§35, §87). OS keychain where available; in-memory fallback for
//! tests. Secrets never enter the editor process as plaintext beyond the minimal
//! credential resolution path (§87).

use crate::PlatformError;

/// Secure storage trait: store, load, delete secrets by key.
pub trait SecureStorage {
    fn store(&self, key: &str, secret: &[u8]) -> Result<(), PlatformError>;
    fn load(&self, key: &str) -> Result<Vec<u8>, PlatformError>;
    fn delete(&self, key: &str) -> Result<(), PlatformError>;
}

/// In-memory secure storage (for tests and headless environments).
pub struct InMemorySecureStorage {
    secrets: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
}

impl InMemorySecureStorage {
    pub fn new() -> Self {
        Self {
            secrets: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl Default for InMemorySecureStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl SecureStorage for InMemorySecureStorage {
    fn store(&self, key: &str, secret: &[u8]) -> Result<(), PlatformError> {
        // A poisoned mutex must not panic the caller: the map is still
        // consistent (no mutation panics mid-update), so recover the guard.
        self.secrets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_string(), secret.to_vec());
        Ok(())
    }
    fn load(&self, key: &str) -> Result<Vec<u8>, PlatformError> {
        self.secrets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
            .ok_or_else(|| PlatformError::NotFound(key.to_string()))
    }
    fn delete(&self, key: &str) -> Result<(), PlatformError> {
        self.secrets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_load_delete_roundtrip() {
        let store = InMemorySecureStorage::new();
        store.store("github_token", b"ghp_abcdef123").unwrap();
        let loaded = store.load("github_token").unwrap();
        assert_eq!(loaded, b"ghp_abcdef123");
        store.delete("github_token").unwrap();
        assert!(store.load("github_token").is_err());
    }

    #[test]
    fn load_missing_key_errors() {
        let store = InMemorySecureStorage::new();
        assert!(matches!(
            store.load("nonexistent"),
            Err(PlatformError::NotFound(_))
        ));
    }

    #[test]
    fn overwrite_existing_key() {
        let store = InMemorySecureStorage::new();
        store.store("key", b"first").unwrap();
        store.store("key", b"second").unwrap();
        assert_eq!(store.load("key").unwrap(), b"second");
    }

    #[test]
    fn delete_missing_key_is_ok() {
        let store = InMemorySecureStorage::new();
        store.delete("nonexistent").unwrap();
    }
}
