use std::error::Error;
use std::future::Future;
use std::pin::Pin;

pub type CompletionResult<'a> =
    Pin<Box<dyn Future<Output = Result<String, Box<dyn Error + Send + Sync>>> + Send + 'a>>;

pub trait ModelBackend: Send + Sync {
    fn complete(&self, prompt: String) -> CompletionResult<'_>;
}
