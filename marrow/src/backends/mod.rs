pub mod ollama;
pub mod openai;

use std::error::Error;

pub(crate) fn describe_network_error(err: &reqwest::Error) -> String {
    let mut kind = "network";
    if err.is_timeout() {
        kind = "timeout";
    } else if err.is_connect() {
        kind = "connection";
    } else if err.is_request() {
        kind = "request";
    } else if err.is_body() {
        kind = "body";
    } else if err.is_decode() {
        kind = "decode";
    } else if err.is_redirect() {
        kind = "redirect";
    }

    let mut detail = String::new();

    // Walk the source chain for low-level details (connection reset, refused, etc.)
    let mut source = err.source();
    while let Some(cause) = source {
        let msg = cause.to_string();
        let lower = msg.to_ascii_lowercase();

        if lower.contains("connection reset") {
            kind = "connection_reset";
        } else if lower.contains("connection refused") {
            kind = "connection_refused";
        } else if lower.contains("connection closed")
            || lower.contains("connection was forcibly closed")
        {
            kind = "connection_closed";
        } else if lower.contains("broken pipe") {
            kind = "broken_pipe";
        } else if lower.contains("timed out") || lower.contains("deadline has elapsed") {
            kind = "timeout";
        } else if lower.contains("dns") || lower.contains("resolve") || lower.contains("no such host") {
            kind = "dns";
        } else if lower.contains("ssl") || lower.contains("tls") || lower.contains("certificate") {
            kind = "tls";
        }

        if !detail.is_empty() {
            detail.push_str(" -> ");
        }
        detail.push_str(&msg);

        source = cause.source();
    }

    if detail.is_empty() {
        detail = err.to_string();
    }

    format!("[{kind}] {detail}")
}
