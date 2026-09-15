/// Redacts strings that resemble credentials before they are written to disk.
pub fn redact_secrets(input: &str) -> String {
    let mut out = input.to_string();
    while let Some(start) = out.find("-----BEGIN") {
        if let Some(rel_end) = out[start..].find("-----END") {
            let end_search = &out[start + rel_end..];
            if let Some(after) = end_search.find("-----") {
                let end = start + rel_end + after + 5;
                out.replace_range(start..end.min(out.len()), "[REDACTED]");
                continue;
            }
        }
        break;
    }

    out = redact_credential_assignments(&out);
    out = redact_tokens_in_string(&out, "Bearer ");
    out = redact_tokens_in_string(&out, "bearer ");
    out = redact_jwts(&out);
    out = redact_token_prefix(&out, "sk-");
    out = redact_token_prefix(&out, "ghp_");
    out = redact_token_prefix(&out, "github_pat_");
    out
}

pub fn redact_credential_assignments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let chars: Vec<(usize, char)> = input.char_indices().collect();
    let len = chars.len();
    let mut idx = 0;

    while idx < len {
        let (byte_pos, ch) = chars[idx];

        // A key can start after start of string or a boundary character
        let is_boundary = idx == 0 || {
            let prev_ch = chars[idx - 1].1;
            prev_ch.is_whitespace() || matches!(prev_ch, '{' | ',' | '(' | '[' | '&' | ';' | '\n')
        };

        if ch == '"' || ch == '\'' {
            // Quoted key e.g. "refresh_token"
            let quote = ch;
            let key_start = idx + 1;
            let mut key_end = key_start;
            while key_end < len && chars[key_end].1 != quote && chars[key_end].1 != '\n' {
                key_end += 1;
            }

            if key_end < len && chars[key_end].1 == quote {
                let key_byte_start = chars[key_start].0;
                let key_byte_end = chars[key_end].0;
                let key_candidate = &input[key_byte_start..key_byte_end];

                if is_credential_key(key_candidate) {
                    // Check if followed by ':' or '=' with optional whitespace
                    let mut after_key = key_end + 1;
                    while after_key < len && chars[after_key].1.is_whitespace() {
                        after_key += 1;
                    }
                    if after_key < len && (chars[after_key].1 == ':' || chars[after_key].1 == '=') {
                        let sep_idx = after_key;
                        let mut val_start = sep_idx + 1;
                        while val_start < len && chars[val_start].1.is_whitespace() {
                            val_start += 1;
                        }
                        if val_start < len {
                            let val_ch = chars[val_start].1;
                            if val_ch == '"' || val_ch == '\'' {
                                // Quoted value: "..."
                                let val_quote = val_ch;
                                let mut val_end = val_start + 1;
                                while val_end < len {
                                    if chars[val_end].1 == '\\' {
                                        val_end += 2;
                                        continue;
                                    }
                                    if chars[val_end].1 == val_quote {
                                        break;
                                    }
                                    val_end += 1;
                                }
                                if val_end < len && chars[val_end].1 == val_quote {
                                    // Append everything up to the value quote content
                                    let before_val_bytes =
                                        chars[val_start].0 + val_quote.len_utf8();
                                    out.push_str(&input[byte_pos..before_val_bytes]);
                                    out.push_str("[REDACTED]");
                                    out.push(val_quote);
                                    idx = val_end + 1;
                                    continue;
                                }
                            } else {
                                // Unquoted value
                                let mut val_end = val_start;
                                while val_end < len {
                                    let vch = chars[val_end].1;
                                    if vch.is_whitespace()
                                        || matches!(vch, ',' | '}' | ']' | ')' | ';' | '&')
                                    {
                                        break;
                                    }
                                    val_end += 1;
                                }
                                if val_end > val_start {
                                    let before_val_bytes = chars[val_start].0;
                                    out.push_str(&input[byte_pos..before_val_bytes]);
                                    out.push_str("[REDACTED]");
                                    idx = val_end;
                                    continue;
                                }
                            }
                        }
                    }
                }
            }
        } else if is_boundary && (ch.is_ascii_alphanumeric() || ch == '_') {
            // Unquoted key candidate e.g. refresh_token= or csrfToken:
            let key_start = idx;
            let mut key_end = key_start;
            while key_end < len
                && (chars[key_end].1.is_ascii_alphanumeric()
                    || chars[key_end].1 == '_'
                    || chars[key_end].1 == '-')
            {
                key_end += 1;
            }
            let key_byte_start = chars[key_start].0;
            let key_byte_end = if key_end < len {
                chars[key_end].0
            } else {
                input.len()
            };
            let key_candidate = &input[key_byte_start..key_byte_end];

            if is_credential_key(key_candidate) {
                let mut after_key = key_end;
                while after_key < len && chars[after_key].1.is_whitespace() {
                    after_key += 1;
                }
                if after_key < len && (chars[after_key].1 == ':' || chars[after_key].1 == '=') {
                    let sep_idx = after_key;
                    let mut val_start = sep_idx + 1;
                    while val_start < len && chars[val_start].1.is_whitespace() {
                        val_start += 1;
                    }
                    if val_start < len {
                        let val_ch = chars[val_start].1;
                        if val_ch == '"' || val_ch == '\'' {
                            let val_quote = val_ch;
                            let mut val_end = val_start + 1;
                            while val_end < len {
                                if chars[val_end].1 == '\\' {
                                    val_end += 2;
                                    continue;
                                }
                                if chars[val_end].1 == val_quote {
                                    break;
                                }
                                val_end += 1;
                            }
                            if val_end < len && chars[val_end].1 == val_quote {
                                let before_val_bytes = chars[val_start].0 + val_quote.len_utf8();
                                out.push_str(&input[byte_pos..before_val_bytes]);
                                out.push_str("[REDACTED]");
                                out.push(val_quote);
                                idx = val_end + 1;
                                continue;
                            }
                        } else {
                            let mut val_end = val_start;
                            while val_end < len {
                                let vch = chars[val_end].1;
                                if vch.is_whitespace()
                                    || matches!(vch, ',' | '}' | ']' | ')' | ';' | '&')
                                {
                                    break;
                                }
                                val_end += 1;
                            }
                            if val_end > val_start {
                                let before_val_bytes = chars[val_start].0;
                                out.push_str(&input[byte_pos..before_val_bytes]);
                                out.push_str("[REDACTED]");
                                idx = val_end;
                                continue;
                            }
                        }
                    }
                }
            }
        }

        out.push(ch);
        idx += 1;
    }

    out
}

fn is_credential_key(raw_key: &str) -> bool {
    let key = raw_key.trim();
    if key.is_empty() {
        return false;
    }
    let norm = key
        .to_ascii_lowercase()
        .replace(['_', '-', ' ', '.', ':'], "");

    if norm.ends_with("count")
        || norm.ends_with("total")
        || norm.ends_with("limit")
        || norm.ends_with("used")
        || norm.ends_with("length")
        || norm.ends_with("len")
        || norm.ends_with("size")
    {
        return false;
    }

    if norm == "token"
        || norm.contains("refreshtoken")
        || norm.contains("accesstoken")
        || norm.contains("csrftoken")
        || norm.contains("xsrftoken")
        || norm.contains("authtoken")
        || norm.contains("idtoken")
        || norm.contains("apitoken")
        || norm.contains("sessiontoken")
        || norm.contains("bearertoken")
        || norm.ends_with("token")
        || norm.starts_with("token")
    {
        return true;
    }

    if norm.contains("secret") {
        return true;
    }

    if norm.contains("password") || norm == "passwd" || norm == "pwd" {
        return true;
    }

    if norm.contains("csrf") || norm.contains("xsrf") {
        return true;
    }

    if norm.contains("apikey") || (norm.contains("api") && norm.contains("key")) {
        return true;
    }

    if norm == "auth"
        || norm.contains("authorization")
        || (norm.starts_with("auth") && !norm.starts_with("author"))
    {
        return true;
    }

    if norm.contains("cookie") {
        return true;
    }

    if norm == "session"
        || norm.contains("sessionid")
        || norm.contains("sessionkey")
        || norm.contains("sessionsecret")
    {
        return true;
    }

    false
}

fn redact_tokens_in_string(input: &str, prefix: &str) -> String {
    let mut out = input.to_string();
    let mut search_from = 0;
    while let Some(idx) = out[search_from..].find(prefix) {
        let start = search_from + idx + prefix.len();
        let mut end = start;
        for (off, ch) in out[start..].char_indices() {
            if ch.is_whitespace() || ch == '"' || ch == '\'' {
                break;
            }
            end = start + off + ch.len_utf8();
        }
        if end > start {
            out.replace_range(start..end, "[REDACTED]");
            search_from = start + "[REDACTED]".len();
        } else {
            search_from = start;
        }
    }
    out
}

fn redact_jwts(input: &str) -> String {
    let mut out = input.to_string();
    let mut search_from = 0;
    while let Some(idx) = out[search_from..].find("eyJ") {
        let start = search_from + idx;
        let mut end = start;
        let mut dots = 0;
        for (off, ch) in out[start..].char_indices() {
            if ch == '.' {
                dots += 1;
            }
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                end = start + off + ch.len_utf8();
            } else {
                break;
            }
        }
        if dots >= 2 && end - start > 20 {
            out.replace_range(start..end, "[REDACTED]");
            search_from = start + "[REDACTED]".len();
        } else {
            search_from = start + 3;
        }
    }
    out
}

fn redact_token_prefix(input: &str, prefix: &str) -> String {
    let mut out = input.to_string();
    let mut search_from = 0;
    while let Some(idx) = out[search_from..].find(prefix) {
        let start = search_from + idx;
        let mut end = start + prefix.len();
        for (off, ch) in out[start + prefix.len()..].char_indices() {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                end = start + prefix.len() + off + ch.len_utf8();
            } else {
                break;
            }
        }
        if end > start + prefix.len() + 8 {
            out.replace_range(start..end, "[REDACTED]");
            search_from = start + "[REDACTED]".len();
        } else {
            search_from = start + prefix.len();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_jwt_and_api_key_shapes() {
        let raw = "token=eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxIn0.signature and sk-testkey12345678901234567890";
        let got = redact_secrets(raw);
        assert!(!got.contains("eyJhbGci"));
        assert!(!got.contains("sk-testkey"));
        assert!(got.contains("[REDACTED]"));
    }

    #[test]
    fn f1_redacts_opaque_tokens_and_credential_assignments() {
        // From review finding F1:
        let raw_json =
            r#"{"refresh_token":"fabricated-refresh-value","csrfToken":"fabricated-csrf-value"}"#;
        let got_json = redact_secrets(raw_json);
        assert!(!got_json.contains("fabricated-refresh-value"));
        assert!(!got_json.contains("fabricated-csrf-value"));
        assert_eq!(
            got_json,
            r#"{"refresh_token":"[REDACTED]","csrfToken":"[REDACTED]"}"#
        );

        let raw_assignments = "refresh_token=fabricated-refresh-value and csrfToken: fabricated-csrf-value and api_key='fabricated-key'";
        let got_assignments = redact_secrets(raw_assignments);
        assert!(!got_assignments.contains("fabricated-refresh-value"));
        assert!(!got_assignments.contains("fabricated-csrf-value"));
        assert!(!got_assignments.contains("fabricated-key"));
        assert!(got_assignments.contains("refresh_token=[REDACTED]"));
        assert!(got_assignments.contains("csrfToken: [REDACTED]"));
        assert!(got_assignments.contains("api_key='[REDACTED]'"));
    }
}
