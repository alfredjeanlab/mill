use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

/// Shared store for ACME HTTP-01 challenge tokens.
///
/// The ACME client inserts tokens when initiating challenges; the HTTP
/// handler reads them to serve `/.well-known/acme-challenge/` requests.
///
/// Uses `std::sync::RwLock` (not tokio) because the HTTP handler reads
/// synchronously — same pattern as `CertCache`.
#[derive(Clone, Default)]
pub struct ChallengeStore {
    inner: Arc<RwLock<HashMap<String, String>>>,
}

impl ChallengeStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a challenge token and its key authorization.
    pub fn insert(&self, token: String, key_authorization: String) {
        self.inner.write().insert(token, key_authorization);
    }

    /// Remove a challenge token.
    pub fn remove(&self, token: &str) {
        self.inner.write().remove(token);
    }

    /// Look up the key authorization for a token.
    pub fn get(&self, token: &str) -> Option<String> {
        self.inner.read().get(token).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_get_remove_lifecycle() {
        let store = ChallengeStore::new();

        assert!(store.get("tok1").is_none());

        store.insert("tok1".to_string(), "auth1".to_string());
        assert_eq!(store.get("tok1").as_deref(), Some("auth1"));

        store.insert("tok2".to_string(), "auth2".to_string());
        assert_eq!(store.get("tok2").as_deref(), Some("auth2"));

        store.remove("tok1");
        assert!(store.get("tok1").is_none());
        assert_eq!(store.get("tok2").as_deref(), Some("auth2"));
    }

    #[test]
    fn clone_shares_state() {
        let store = ChallengeStore::new();
        let clone = store.clone();

        store.insert("tok".to_string(), "auth".to_string());
        assert_eq!(clone.get("tok").as_deref(), Some("auth"));
    }
}
