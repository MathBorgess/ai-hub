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

    out = redact_tokens_in_string(&out, "Bearer ");
    out = redact_tokens_in_string(&out, "bearer ");
    out = redact_jwts(&out);
    out = redact_token_prefix(&out, "sk-");
    out = redact_token_prefix(&out, "ghp_");
    out = redact_token_prefix(&out, "github_pat_");
    out
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
}
