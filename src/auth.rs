//! Authentication helpers for Confluence REST API access.
//!
//! Resolves Bearer (personal access token) or Basic (Cloud username + API token)
//! credentials from environment variables and builds request headers.

use anyhow::{Result, bail};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};

/// Reads authentication environment variables and returns the full
/// Authorization header value string.
///
/// Priority:
/// 1. If CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN is set (non-empty, non-whitespace) → "Bearer <token>"
/// 2. If both CONFLUENCE2MD_USERNAME and CONFLUENCE2MD_API_TOKEN are set → "Basic <base64>"
/// 3. Otherwise → error
pub fn resolve_auth() -> Result<String> {
    let pat = std::env::var("CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN").ok();
    let username = std::env::var("CONFLUENCE2MD_USERNAME").ok();
    let api_token = std::env::var("CONFLUENCE2MD_API_TOKEN").ok();

    // 1. Check PAT first
    if let Some(ref pat_val) = pat
        && !pat_val.is_empty()
    {
        // PAT is set (non-empty) — validate it's not whitespace-only
        if pat_val.trim().is_empty() {
            bail!("CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN is set but empty or whitespace-only.");
        }
        return Ok(format!("Bearer {pat_val}"));
    }

    // 2. PAT is unset or empty — check Cloud pair
    let has_username = matches!(&username, Some(u) if !u.is_empty());
    let has_api_token = matches!(&api_token, Some(t) if !t.is_empty());

    if has_username && has_api_token {
        // 3. Both Cloud vars set — validate neither is whitespace-only
        let u = username.unwrap();
        let t = api_token.unwrap();
        if u.trim().is_empty() {
            bail!("CONFLUENCE2MD_USERNAME must not be empty or whitespace-only.");
        }
        if t.trim().is_empty() {
            bail!("CONFLUENCE2MD_API_TOKEN must not be empty or whitespace-only.");
        }
        let credentials = format!("{u}:{t}");
        let encoded = base64_encode(credentials.as_bytes());
        return Ok(format!("Basic {encoded}"));
    }

    // 4. Exactly one Cloud var set — error naming the missing one
    if has_username && !has_api_token {
        bail!(
            "CONFLUENCE2MD_USERNAME is set but CONFLUENCE2MD_API_TOKEN is missing. \
             Both are required for Cloud authentication."
        );
    }
    if has_api_token && !has_username {
        bail!(
            "CONFLUENCE2MD_API_TOKEN is set but CONFLUENCE2MD_USERNAME is missing. \
             Both are required for Cloud authentication."
        );
    }

    // 5. Nothing set at all
    bail!(
        "No authentication configured. Set CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN for Bearer auth, \
         or both CONFLUENCE2MD_USERNAME and CONFLUENCE2MD_API_TOKEN for Basic auth."
    );
}

// ── Base64 encoding ────────────────────────────────────────────────

/// RFC 4648 standard Base64 encoding (no line breaks, standard alphabet with padding).
pub(crate) fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let chunks = input.chunks(3);

    for chunk in chunks {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };

        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);

        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }

        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }

    out
}

// ── HTTP helpers ───────────────────────────────────────────────────

pub(crate) fn auth_headers(auth_value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    if let Ok(v) = HeaderValue::from_str(auth_value) {
        headers.insert(AUTHORIZATION, v);
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::{auth_headers, base64_encode, resolve_auth};
    use reqwest::header::{ACCEPT, AUTHORIZATION};

    // Feature: cloud-api-token-auth, Property 1: base64 encoding correctness

    #[test]
    fn base64_encode_empty_input() {
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn base64_encode_rfc4648_test_vectors() {
        // RFC 4648 §10 test vectors
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_encode_confluence_credential_example() {
        // Typical Cloud auth: email:api-token
        let input = b"user@example.com:my-api-token";
        assert_eq!(
            base64_encode(input),
            "dXNlckBleGFtcGxlLmNvbTpteS1hcGktdG9rZW4="
        );
    }

    #[test]
    fn base64_encode_non_ascii_utf8() {
        // Non-ASCII UTF-8 bytes
        let input = "ü:tökën".as_bytes();
        assert_eq!(base64_encode(input), "w7w6dMO2a8Orbg==");
    }

    #[test]
    fn base64_encode_all_zero_bytes() {
        assert_eq!(base64_encode(&[0, 0, 0]), "AAAA");
        assert_eq!(base64_encode(&[0]), "AA==");
        assert_eq!(base64_encode(&[0, 0]), "AAA=");
    }

    #[test]
    fn base64_encode_all_ff_bytes() {
        assert_eq!(base64_encode(&[0xFF, 0xFF, 0xFF]), "////");
    }

    /// Validates: Requirements 3.1, 3.4, 6.1
    /// Explicitly verifies all padding cases based on input length mod 3:
    /// - len % 3 == 0 → no padding
    /// - len % 3 == 1 → two `=` padding chars
    /// - len % 3 == 2 → one `=` padding char
    #[test]
    fn base64_encode_padding_cases_by_length_mod_3() {
        // len % 3 == 0: no padding (lengths 3, 6)
        let no_pad_3 = base64_encode(b"abc");
        assert!(
            !no_pad_3.ends_with('='),
            "len=3 should have no padding, got: {no_pad_3}"
        );
        assert_eq!(no_pad_3, "YWJj");

        let no_pad_6 = base64_encode(b"abcdef");
        assert!(
            !no_pad_6.ends_with('='),
            "len=6 should have no padding, got: {no_pad_6}"
        );
        assert_eq!(no_pad_6, "YWJjZGVm");

        // len % 3 == 1: two padding chars (lengths 1, 4)
        let two_pad_1 = base64_encode(b"a");
        assert!(
            two_pad_1.ends_with("=="),
            "len=1 should have == padding, got: {two_pad_1}"
        );
        assert_eq!(two_pad_1, "YQ==");

        let two_pad_4 = base64_encode(b"abcd");
        assert!(
            two_pad_4.ends_with("=="),
            "len=4 should have == padding, got: {two_pad_4}"
        );
        assert_eq!(two_pad_4, "YWJjZA==");

        // len % 3 == 2: one padding char (lengths 2, 5)
        let one_pad_2 = base64_encode(b"ab");
        assert!(
            one_pad_2.ends_with('=') && !one_pad_2.ends_with("=="),
            "len=2 should have single = padding, got: {one_pad_2}"
        );
        assert_eq!(one_pad_2, "YWI=");

        let one_pad_5 = base64_encode(b"abcde");
        assert!(
            one_pad_5.ends_with('=') && !one_pad_5.ends_with("=="),
            "len=5 should have single = padding, got: {one_pad_5}"
        );
        assert_eq!(one_pad_5, "YWJjZGU=");
    }

    // Feature: cloud-api-token-auth, Property 1: base64 encoding correctness
    // Validates: Requirements 3.1, 3.4, 6.1

    /// Programmatic property coverage: verify all single-byte values (0x00..=0xFF)
    /// produce output containing only valid base64 characters and the expected length.
    #[test]
    fn base64_encode_property_all_single_bytes() {
        let valid_chars = |s: &str| -> bool {
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
        };

        for byte in 0x00u8..=0xFF {
            let input = [byte];
            let output = base64_encode(&input);

            // Single-byte input → output length must be 4 (4 * ceil(1/3) = 4)
            assert_eq!(
                output.len(),
                4,
                "byte 0x{byte:02X}: expected output length 4, got {}",
                output.len()
            );

            // Output must only contain valid base64 characters
            assert!(
                valid_chars(&output),
                "byte 0x{byte:02X}: output contains invalid base64 chars: {output}"
            );
        }
    }

    /// Programmatic property coverage: verify inputs of length 0..=4 produce correct
    /// output length and only valid base64 characters, covering all padding cases.
    #[test]
    fn base64_encode_property_lengths_0_through_4() {
        let valid_chars = |s: &str| -> bool {
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
        };

        // Expected output lengths: 4 * ceil(input_len / 3)
        // len 0 → 0, len 1 → 4, len 2 → 4, len 3 → 4, len 4 → 8
        let expected_output_lengths: [usize; 5] = [0, 4, 4, 4, 8];

        // Use representative byte patterns for each length
        let test_patterns: &[&[u8]] = &[
            &[],                       // length 0
            &[0x00],                   // length 1 (min byte)
            &[0x7F, 0x80],             // length 2 (boundary bytes)
            &[0xFF, 0x00, 0xAB],       // length 3 (mixed)
            &[0x01, 0x02, 0x03, 0x04], // length 4
        ];

        for pattern in test_patterns {
            let output = base64_encode(pattern);
            let expected_len = expected_output_lengths[pattern.len()];

            assert_eq!(
                output.len(),
                expected_len,
                "input len {}: expected output length {expected_len}, got {}. input: {pattern:?}",
                pattern.len(),
                output.len()
            );

            assert!(
                valid_chars(&output),
                "input len {}: output contains invalid base64 chars: {output}. input: {pattern:?}",
                pattern.len()
            );
        }

        // Additional patterns per length to cover more byte diversity
        let additional_patterns: &[&[u8]] = &[
            // More length 1 examples
            &[0xFF],
            &[0x41], // 'A'
            &[0x80],
            // More length 2 examples
            &[0x00, 0x00],
            &[0xFF, 0xFF],
            &[0x41, 0x42],
            // More length 3 examples
            &[0x00, 0x00, 0x00],
            &[0xFF, 0xFF, 0xFF],
            &[0x41, 0x42, 0x43],
            // More length 4 examples
            &[0x00, 0x00, 0x00, 0x00],
            &[0xFF, 0xFF, 0xFF, 0xFF],
            &[0xDE, 0xAD, 0xBE, 0xEF],
        ];

        for pattern in additional_patterns {
            let output = base64_encode(pattern);
            let expected_len = expected_output_lengths[pattern.len()];

            assert_eq!(
                output.len(),
                expected_len,
                "input len {}: expected output length {expected_len}, got {}. input: {pattern:?}",
                pattern.len(),
                output.len()
            );

            assert!(
                valid_chars(&output),
                "input len {}: output contains invalid base64 chars: {output}. input: {pattern:?}",
                pattern.len()
            );
        }
    }

    // Feature: cloud-api-token-auth — auth_headers tests
    // Validates: Requirements 3.2, 4.2

    #[test]
    fn auth_headers_bearer_sets_authorization_and_accept() {
        let headers = auth_headers("Bearer my-token");
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer my-token"
        );
        assert_eq!(
            headers.get(ACCEPT).unwrap().to_str().unwrap(),
            "application/json"
        );
    }

    #[test]
    fn auth_headers_basic_sets_authorization_and_accept() {
        let headers = auth_headers("Basic dXNlcjpwYXNz");
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Basic dXNlcjpwYXNz"
        );
        assert_eq!(
            headers.get(ACCEPT).unwrap().to_str().unwrap(),
            "application/json"
        );
    }

    // ── resolve_auth() unit tests ──────────────────────────────────────
    //
    // Feature: cloud-api-token-auth
    // Validates: Requirements 1.3, 1.4, 2.1, 2.2, 2.3, 2.4, 2.5, 4.1, 4.3, 7.2, 7.3
    //
    // Environment variables are process-global, so these tests must not run
    // concurrently.  We use a static Mutex to serialize access.

    use std::sync::Mutex;

    static AUTH_ENV_LOCK: Mutex<()> = Mutex::new(());

    const PAT_VAR: &str = "CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN";
    const USER_VAR: &str = "CONFLUENCE2MD_USERNAME";
    const TOKEN_VAR: &str = "CONFLUENCE2MD_API_TOKEN";

    /// Helper: clear all auth env vars, returning the lock guard.
    ///
    /// # Safety
    /// Env var manipulation is unsafe in Rust 2024 edition because it is not
    /// thread-safe. We serialize access via AUTH_ENV_LOCK so concurrent
    /// modification cannot occur within our test suite.
    fn clear_auth_env() -> std::sync::MutexGuard<'static, ()> {
        let guard = AUTH_ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by AUTH_ENV_LOCK — no concurrent env mutation.
        unsafe {
            std::env::remove_var(PAT_VAR);
            std::env::remove_var(USER_VAR);
            std::env::remove_var(TOKEN_VAR);
        }
        guard
    }

    #[test]
    fn resolve_auth_pat_set_alone_returns_bearer() {
        let _guard = clear_auth_env();
        // SAFETY: guarded by AUTH_ENV_LOCK.
        unsafe { std::env::set_var(PAT_VAR, "my-pat-value") };

        let result = resolve_auth().unwrap();
        assert_eq!(result, "Bearer my-pat-value");
    }

    #[test]
    fn resolve_auth_cloud_vars_set_no_pat_returns_basic() {
        let _guard = clear_auth_env();
        // SAFETY: guarded by AUTH_ENV_LOCK.
        unsafe {
            std::env::set_var(USER_VAR, "user@example.com");
            std::env::set_var(TOKEN_VAR, "my-api-token");
        }

        let result = resolve_auth().unwrap();
        // base64("user@example.com:my-api-token") = "dXNlckBleGFtcGxlLmNvbTpteS1hcGktdG9rZW4="
        assert_eq!(result, "Basic dXNlckBleGFtcGxlLmNvbTpteS1hcGktdG9rZW4=");
    }

    #[test]
    fn resolve_auth_both_cloud_and_pat_set_returns_bearer_priority() {
        let _guard = clear_auth_env();
        // SAFETY: guarded by AUTH_ENV_LOCK.
        unsafe {
            std::env::set_var(PAT_VAR, "my-pat");
            std::env::set_var(USER_VAR, "user@example.com");
            std::env::set_var(TOKEN_VAR, "my-api-token");
        }

        let result = resolve_auth().unwrap();
        assert_eq!(result, "Bearer my-pat");
    }

    #[test]
    fn resolve_auth_no_vars_set_returns_error_listing_both_options() {
        let _guard = clear_auth_env();

        let err = resolve_auth().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN"),
            "error should mention PAT var: {msg}"
        );
        assert!(
            msg.contains("CONFLUENCE2MD_USERNAME") || msg.contains("CONFLUENCE2MD_API_TOKEN"),
            "error should mention Cloud vars: {msg}"
        );
    }

    #[test]
    fn resolve_auth_only_username_set_errors_naming_missing_api_token() {
        let _guard = clear_auth_env();
        // SAFETY: guarded by AUTH_ENV_LOCK.
        unsafe { std::env::set_var(USER_VAR, "user@example.com") };

        let err = resolve_auth().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("CONFLUENCE2MD_API_TOKEN"),
            "error should name missing API token var: {msg}"
        );
    }

    #[test]
    fn resolve_auth_only_api_token_set_errors_naming_missing_username() {
        let _guard = clear_auth_env();
        // SAFETY: guarded by AUTH_ENV_LOCK.
        unsafe { std::env::set_var(TOKEN_VAR, "my-api-token") };

        let err = resolve_auth().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("CONFLUENCE2MD_USERNAME"),
            "error should name missing username var: {msg}"
        );
    }

    #[test]
    fn resolve_auth_pat_whitespace_only_returns_error() {
        let _guard = clear_auth_env();
        // SAFETY: guarded by AUTH_ENV_LOCK.
        unsafe { std::env::set_var(PAT_VAR, "   \t  ") };

        let err = resolve_auth().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("whitespace") || msg.contains("empty"),
            "error should mention whitespace/empty PAT: {msg}"
        );
    }

    #[test]
    fn resolve_auth_username_whitespace_only_with_valid_api_token_returns_error() {
        let _guard = clear_auth_env();
        // SAFETY: guarded by AUTH_ENV_LOCK.
        unsafe {
            std::env::set_var(USER_VAR, "   ");
            std::env::set_var(TOKEN_VAR, "valid-token");
        }

        let err = resolve_auth().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("USERNAME") || msg.contains("username"),
            "error should mention username issue: {msg}"
        );
    }

    #[test]
    fn resolve_auth_api_token_whitespace_only_with_valid_username_returns_error() {
        let _guard = clear_auth_env();
        // SAFETY: guarded by AUTH_ENV_LOCK.
        unsafe {
            std::env::set_var(USER_VAR, "user@example.com");
            std::env::set_var(TOKEN_VAR, "   \t");
        }

        let err = resolve_auth().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("API_TOKEN") || msg.contains("api_token") || msg.contains("API token"),
            "error should mention API token issue: {msg}"
        );
    }

    // Feature: cloud-api-token-auth, Property 2: PAT priority and Bearer header format
    // **Validates: Requirements 2.1, 2.4, 4.1, 4.2**
    #[test]
    fn resolve_auth_pat_priority_and_bearer_format_programmatic() {
        let long_pat = "a".repeat(100);
        let pat_values_owned: Vec<&str> = vec![
            "x",
            &long_pat,
            "abc!@#$%^&*()_+-=[]{}",
            "tökën-pät",
            "token with spaces",
        ];

        for pat_val in &pat_values_owned {
            // Case A: PAT set alone (no cloud vars)
            {
                let _guard = clear_auth_env();
                // SAFETY: guarded by AUTH_ENV_LOCK.
                unsafe {
                    std::env::set_var(PAT_VAR, pat_val);
                }

                let result = resolve_auth();
                assert!(
                    result.is_ok(),
                    "PAT alone should succeed for pat={pat_val:?}, got err: {:?}",
                    result.unwrap_err()
                );
                let header = result.unwrap();
                assert!(
                    header.starts_with("Bearer "),
                    "Header must start with 'Bearer ' for pat={pat_val:?}, got: {header:?}"
                );
                let remainder = &header["Bearer ".len()..];
                assert_eq!(
                    remainder, *pat_val,
                    "Remainder after 'Bearer ' must equal exact PAT value for pat={pat_val:?}"
                );
            }

            // Case B: PAT set WITH cloud vars also set (PAT takes priority)
            {
                let _guard = clear_auth_env();
                // SAFETY: guarded by AUTH_ENV_LOCK.
                unsafe {
                    std::env::set_var(PAT_VAR, pat_val);
                    std::env::set_var(USER_VAR, "user@example.com");
                    std::env::set_var(TOKEN_VAR, "token123");
                }

                let result = resolve_auth();
                assert!(
                    result.is_ok(),
                    "PAT with cloud vars should succeed for pat={pat_val:?}, got err: {:?}",
                    result.unwrap_err()
                );
                let header = result.unwrap();
                assert!(
                    header.starts_with("Bearer "),
                    "Header must start with 'Bearer ' when PAT has priority for pat={pat_val:?}, got: {header:?}"
                );
                let remainder = &header["Bearer ".len()..];
                assert_eq!(
                    remainder, *pat_val,
                    "Remainder after 'Bearer ' must equal exact PAT value (priority over cloud) for pat={pat_val:?}"
                );
            }
        }
    }

    // Feature: cloud-api-token-auth, Property 3: Cloud Basic auth header format
    // **Validates: Requirements 1.5, 2.2, 3.1, 3.4**
    //
    // For any pair of non-empty, non-whitespace-only values for username and
    // api_token, when PAT is not set, resolve_auth() returns "Basic <encoded>"
    // where <encoded> == base64_encode("{username}:{api_token}".as_bytes()).
    //
    // This test loops over ASCII and non-ASCII username/token pairs, including
    // colons in the token (edge case for the separator), and verifies:
    // - Result is Ok
    // - Result starts with "Basic "
    // - The base64 portion only contains valid base64 chars [A-Za-z0-9+/=]
    // - The base64 portion matches base64_encode("{username}:{api_token}".as_bytes())
    #[test]
    fn resolve_auth_basic_header_format_property_coverage() {
        let long_user = "a".repeat(50) + "@example.com";
        let long_token = "b".repeat(100);

        let pairs: Vec<(&str, &str)> = vec![
            // Normal ASCII
            ("user@example.com", "simple-token"),
            // Unicode username
            ("üser@example.com", "token123"),
            // Unicode token
            ("user@example.com", "tökën-123"),
            // Both unicode
            ("ü@ëx.com", "tök:ën"),
            // Colon in token (edge case for the separator)
            ("user@example.com", "token:with:colons"),
            // Special chars
            ("user+tag@example.com", "abc!@#$%^&*()"),
        ];

        // Also include long values (owned strings)
        let owned_pairs: Vec<(String, String)> = vec![(long_user.clone(), long_token.clone())];

        // Combine into a single iteration
        let all_pairs: Vec<(&str, &str)> = pairs
            .iter()
            .copied()
            .chain(owned_pairs.iter().map(|(u, t)| (u.as_str(), t.as_str())))
            .collect();

        for (username, api_token) in &all_pairs {
            let _guard = clear_auth_env();
            // SAFETY: guarded by AUTH_ENV_LOCK.
            unsafe {
                std::env::set_var(USER_VAR, username);
                std::env::set_var(TOKEN_VAR, api_token);
            }

            let result = resolve_auth();
            assert!(
                result.is_ok(),
                "resolve_auth() should succeed for ({username:?}, {api_token:?}), got: {:?}",
                result.err()
            );

            let header_value = result.unwrap();

            // Must start with "Basic "
            assert!(
                header_value.starts_with("Basic "),
                "Header for ({username:?}, {api_token:?}) should start with 'Basic ', got: {header_value}"
            );

            // Extract the base64 portion
            let b64_portion = &header_value["Basic ".len()..];

            // Verify base64 alphabet: only [A-Za-z0-9+/=]
            assert!(
                b64_portion
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='),
                "Base64 portion for ({username:?}, {api_token:?}) contains invalid chars: {b64_portion}"
            );

            // Verify correctness: re-encode expected input and compare
            let expected_input = format!("{username}:{api_token}");
            let expected_b64 = base64_encode(expected_input.as_bytes());
            assert_eq!(
                b64_portion, expected_b64,
                "Base64 mismatch for ({username:?}, {api_token:?}): got {b64_portion}, expected {expected_b64}"
            );
        }
    }
}
