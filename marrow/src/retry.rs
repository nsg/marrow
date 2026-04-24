use std::error::Error;
use std::future::Future;
use std::time::Duration;

use tokio::time::sleep;

/// Configuration for retry with exponential backoff.
pub struct RetryConfig {
    /// Maximum number of retry attempts (not counting the initial attempt).
    pub max_retries: u32,
    /// Initial delay before the first retry.
    pub initial_delay: Duration,
    /// Multiplier applied to the delay after each retry.
    pub multiplier: u32,
    /// Maximum delay cap.
    pub max_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_secs(1),
            multiplier: 2,
            max_delay: Duration::from_secs(30),
        }
    }
}

/// Retry an async operation with exponential backoff.
///
/// Returns immediately on success or on a non-retryable error.
/// Logs each retry attempt to stderr.
pub async fn retry_with_backoff<F, Fut, T>(
    config: &RetryConfig,
    should_retry: impl Fn(&str) -> bool,
    mut operation: F,
) -> Result<T, Box<dyn Error + Send + Sync>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Box<dyn Error + Send + Sync>>>,
{
    let mut delay = config.initial_delay;

    for attempt in 0..=config.max_retries {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(e) => {
                let is_last = attempt == config.max_retries;
                if is_last || !should_retry(&e.to_string()) {
                    return Err(e);
                }

                eprintln!(
                    "[marrow] retry {}/{} after: {}, waiting {}s",
                    attempt + 1,
                    config.max_retries,
                    e,
                    delay.as_secs_f32(),
                );

                sleep(delay).await;
                delay = (delay * config.multiplier).min(config.max_delay);
            }
        }
    }

    unreachable!()
}

/// Returns `true` if the error message indicates a transient failure worth
/// retrying (HTTP 5xx, 429 rate-limit, or network-level errors).
pub fn is_retryable_error(error_msg: &str) -> bool {
    // Match HTTP 5xx status codes from the backends' error format:
    //   "openai returned 500 Internal Server Error: ..."
    //   "ollama returned 502 Bad Gateway: ..."
    if let Some(pos) = error_msg.find("returned ") {
        let after = &error_msg[pos + 9..];
        if after.starts_with('5') {
            return true;
        }
        // 429 Too Many Requests — backoff gives the server time to recover.
        if after.starts_with("429") {
            return true;
        }
    }

    // Network-level errors from reqwest (connection refused, timeout, DNS
    // failure, etc.) are propagated via `?` before the status check.
    let lower = error_msg.to_lowercase();
    let network_indicators = [
        "connection refused",
        "timed out",
        "timeout",
        "dns error",
        "connection reset",
        "broken pipe",
    ];
    network_indicators.iter().any(|ind| lower.contains(ind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn test_no_retry_on_success() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();

        let config = RetryConfig::default();
        let result = retry_with_backoff(&config, is_retryable_error, || {
            let c = calls_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok::<_, Box<dyn Error + Send + Sync>>("ok".to_string())
            }
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_retry_then_success() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();

        let config = RetryConfig {
            max_retries: 3,
            initial_delay: Duration::from_millis(1),
            multiplier: 2,
            max_delay: Duration::from_secs(1),
        };

        let result = retry_with_backoff(&config, is_retryable_error, || {
            let c = calls_clone.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err("openai returned 500 Internal Server Error: overloaded"
                        .to_string()
                        .into())
                } else {
                    Ok::<_, Box<dyn Error + Send + Sync>>("ok".to_string())
                }
            }
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 3); // 2 failures + 1 success
    }

    #[tokio::test]
    async fn test_no_retry_on_client_error() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();

        let config = RetryConfig {
            initial_delay: Duration::from_millis(1),
            ..RetryConfig::default()
        };

        let result: Result<String, _> = retry_with_backoff(&config, is_retryable_error, || {
            let c = calls_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err("openai returned 401 Unauthorized: bad key".into())
            }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1); // no retries
    }

    #[tokio::test]
    async fn test_retries_exhausted() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();

        let config = RetryConfig {
            max_retries: 3,
            initial_delay: Duration::from_millis(1),
            multiplier: 2,
            max_delay: Duration::from_secs(1),
        };

        let result: Result<String, _> = retry_with_backoff(&config, is_retryable_error, || {
            let c = calls_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err("openai returned 503 Service Unavailable: try later".into())
            }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 4); // 1 initial + 3 retries
    }

    #[tokio::test]
    async fn test_retry_on_network_error() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();

        let config = RetryConfig {
            max_retries: 2,
            initial_delay: Duration::from_millis(1),
            multiplier: 2,
            max_delay: Duration::from_secs(1),
        };

        let result = retry_with_backoff(&config, is_retryable_error, || {
            let c = calls_clone.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err("connection refused".into())
                } else {
                    Ok::<_, Box<dyn Error + Send + Sync>>("recovered".to_string())
                }
            }
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_is_retryable_error() {
        // 5xx errors — retryable
        assert!(is_retryable_error(
            "openai returned 500 Internal Server Error: overloaded"
        ));
        assert!(is_retryable_error(
            "ollama returned 502 Bad Gateway: upstream down"
        ));
        assert!(is_retryable_error(
            "openai returned 503 Service Unavailable: try later"
        ));

        // 429 rate limit — retryable
        assert!(is_retryable_error(
            "openai returned 429 Too Many Requests: slow down"
        ));

        // 4xx client errors — not retryable
        assert!(!is_retryable_error(
            "openai returned 401 Unauthorized: bad key"
        ));
        assert!(!is_retryable_error(
            "openai returned 400 Bad Request: invalid model"
        ));
        assert!(!is_retryable_error(
            "openai returned 404 Not Found: no such model"
        ));

        // Network errors — retryable
        assert!(is_retryable_error("connection refused"));
        assert!(is_retryable_error("request timed out"));
        assert!(is_retryable_error("Connection reset by peer"));
        assert!(is_retryable_error("DNS error: name not resolved"));

        // Unrelated errors — not retryable
        assert!(!is_retryable_error("no choices in response"));
        assert!(!is_retryable_error("invalid JSON"));
    }
}
