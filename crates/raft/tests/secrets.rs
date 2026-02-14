use mill_raft::secrets::ClusterKey;

#[test]
fn test_encrypt_decrypt_roundtrip() {
    let key = ClusterKey::generate();
    let plaintext = b"super-secret-database-password";

    let (ciphertext, nonce) = key.encrypt(plaintext).unwrap();
    assert_ne!(ciphertext, plaintext);

    let decrypted = key.decrypt(&ciphertext, &nonce).unwrap();
    assert_eq!(decrypted, plaintext);
}

#[test]
fn test_encrypt_empty() {
    let key = ClusterKey::generate();
    let (ciphertext, nonce) = key.encrypt(b"").unwrap();
    let decrypted = key.decrypt(&ciphertext, &nonce).unwrap();
    assert_eq!(decrypted, b"");
}

#[test]
fn test_wrong_key_fails() {
    let key1 = ClusterKey::generate();
    let key2 = ClusterKey::generate();

    let plaintext = b"secret";
    let (ciphertext, nonce) = key1.encrypt(plaintext).unwrap();

    let result = key2.decrypt(&ciphertext, &nonce);
    assert!(result.is_err());
}

#[test]
fn test_from_bytes() {
    let bytes = [42u8; 32];
    let key = ClusterKey::from_bytes(&bytes);

    let plaintext = b"hello world";
    let (ciphertext, nonce) = key.encrypt(plaintext).unwrap();
    let decrypted = key.decrypt(&ciphertext, &nonce).unwrap();
    assert_eq!(decrypted, plaintext);
}

#[test]
fn test_as_bytes_round_trip() {
    let key = ClusterKey::generate();
    let bytes = key.as_bytes();
    assert_eq!(bytes.len(), 32);

    let bytes_array: [u8; 32] = bytes.try_into().unwrap();
    let restored = ClusterKey::from_bytes(&bytes_array);

    let plaintext = b"persistence test";
    let (ciphertext, nonce) = key.encrypt(plaintext).unwrap();
    let decrypted = restored.decrypt(&ciphertext, &nonce).unwrap();
    assert_eq!(decrypted, plaintext);
}

#[test]
fn test_unique_nonces() {
    let key = ClusterKey::generate();
    let plaintext = b"same input";

    let (_, nonce1) = key.encrypt(plaintext).unwrap();
    let (_, nonce2) = key.encrypt(plaintext).unwrap();

    // Nonces must be unique for security
    assert_ne!(nonce1, nonce2);
}
