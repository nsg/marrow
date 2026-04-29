use std::error::Error;
use std::fmt;
use std::future::Future;
use std::time::Duration;

use tokio::time::sleep;

/// What the retry loop should do after a failed attempt.
pub enum Retry {
    /// Not retryable — fail immediately.
    Fail,
    /// Retryable — use exponential backoff.
    Backoff,
    /// Retryable — wait a specific duration (e.g. from a Retry-After header).
    After(Duration),
}

/// Structured error from a backend HTTP call.
#[derive(Debug)]
pub enum BackendError {
    /// Network-level failure (connection refused, DNS, TLS, timeout, etc.)
    Network(Box<dyn Error + Send + Sync>),
    /// HTTP response with a non-success status code.
    Http {
        status: u16,
        body: String,
        retry_after: Option<Duration>,
    },
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Network(e) => write!(f, "network error: {e}"),
            Self::Http { status, body, .. } => write!(f, "HTTP {status}: {body}"),
        }
    }
}

impl Error for BackendError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Network(e) => Some(e.as_ref()),
            Self::Http { .. } => None,
        }
    }
}

impl BackendError {
    /// How the retry loop should handle this error.
    pub fn should_retry(&self) -> Retry {
        match self {
            Self::Network(_) => Retry::Backoff,
            Self::Http {
                status,
                retry_after,
                ..
            } => {
                if *status == 429 {
                    match retry_after {
                        Some(d) => Retry::After(*d),
                        None => Retry::Backoff,
                    }
                } else if (500..600).contains(status) {
                    Retry::Backoff
                } else {
                    Retry::Fail
                }
            }
        }
    }
}

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
pub async fn retry_with_backoff<F, Fut, T, E>(
    config: &RetryConfig,
    should_retry: impl Fn(&E) -> Retry,
    mut operation: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: fmt::Display,
{
    let mut delay = config.initial_delay;

    for attempt in 0..=config.max_retries {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(e) => {
                let is_last = attempt == config.max_retries;
                let wait = match should_retry(&e) {
                    Retry::Fail => return Err(e),
                    _ if is_last => return Err(e),
                    Retry::Backoff => delay,
                    Retry::After(d) => d,
                };

                eprintln!(
                    "[marrow] retry {}/{} after: {}, waiting {}s",
                    attempt + 1,
                    config.max_retries,
                    e,
                    wait.as_secs_f32(),
                );

                sleep(wait).await;
                delay = (delay * config.multiplier).min(config.max_delay);
            }
        }
    }

    unreachable!()
}

/// Parse a `Retry-After` header value as a number of seconds.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
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
        let result = retry_with_backoff(&config, BackendError::should_retry, || {
            let c = calls_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok::<_, BackendError>("ok".to_string())
            }
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_retry_on_server_error() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();

        let config = RetryConfig {
            max_retries: 3,
            initial_delay: Duration::from_millis(1),
            multiplier: 2,
            max_delay: Duration::from_secs(1),
        };

        let result = retry_with_backoff(&config, BackendError::should_retry, || {
            let c = calls_clone.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err(BackendError::Http {
                        status: 500,
                        body: "overloaded".into(),
                        retry_after: None,
                    })
                } else {
                    Ok::<_, BackendError>("ok".to_string())
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

        let result: Result<String, _> =
            retry_with_backoff(&config, BackendError::should_retry, || {
                let c = calls_clone.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Err(BackendError::Http {
                        status: 401,
                        body: "bad key".into(),
                        retry_after: None,
                    })
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

        let result: Result<String, _> =
            retry_with_backoff(&config, BackendError::should_retry, || {
                let c = calls_clone.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Err(BackendError::Http {
                        status: 503,
                        body: "try later".into(),
                        retry_after: None,
                    })
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

        let result = retry_with_backoff(&config, BackendError::should_retry, || {
            let c = calls_clone.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err(BackendError::Network("connection refused".into()))
                } else {
                    Ok::<_, BackendError>("recovered".to_string())
                }
            }
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_retry_429_uses_retry_after() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();

        let config = RetryConfig {
            max_retries: 2,
            initial_delay: Duration::from_millis(1),
            multiplier: 2,
            max_delay: Duration::from_secs(1),
        };

        let start = std::time::Instant::now();
        let result = retry_with_backoff(&config, BackendError::should_retry, || {
            let c = calls_clone.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err(BackendError::Http {
                        status: 429,
                        body: "slow down".into(),
                        // Server says wait 100ms — not the backoff's 1ms
                        retry_after: Some(Duration::from_millis(100)),
                    })
                } else {
                    Ok::<_, BackendError>("ok".to_string())
                }
            }
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // Should have waited ~100ms (the Retry-After), not ~1ms (the backoff)
        assert!(start.elapsed() >= Duration::from_millis(80));
    }

    #[test]
    fn test_should_retry() {
        // Network errors — always backoff
        assert!(matches!(
            BackendError::Network("connection refused".into()).should_retry(),
            Retry::Backoff
        ));

        // 5xx — backoff
        assert!(matches!(
            BackendError::Http {
                status: 502,
                body: "bad gateway".into(),
                retry_after: None,
            }
            .should_retry(),
            Retry::Backoff
        ));

        // 429 without Retry-After — backoff
        assert!(matches!(
            BackendError::Http {
                status: 429,
                body: "slow down".into(),
                retry_after: None,
            }
            .should_retry(),
            Retry::Backoff
        ));

        // 429 with Retry-After — use the specified delay
        let retry = BackendError::Http {
            status: 429,
            body: "slow down".into(),
            retry_after: Some(Duration::from_secs(5)),
        }
        .should_retry();
        match retry {
            Retry::After(d) => assert_eq!(d, Duration::from_secs(5)),
            _ => panic!("expected Retry::After"),
        }

        // 4xx — fail
        assert!(matches!(
            BackendError::Http {
                status: 401,
                body: "bad key".into(),
                retry_after: None,
            }
            .should_retry(),
            Retry::Fail
        ));
    }

    #[test]
    fn test_parse_retry_after() {
        assert_eq!(parse_retry_after("5"), Some(Duration::from_secs(5)));
        assert_eq!(parse_retry_after(" 30 "), Some(Duration::from_secs(30)));
        assert_eq!(parse_retry_after("0"), Some(Duration::from_secs(0)));
        // HTTP-date format — not supported, returns None (falls back to backoff)
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after("garbage"), None);
    }
}
