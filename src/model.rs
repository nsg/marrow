use std::error::Error;
use std::future::Future;
use std::pin::Pin;

use crate::session::Message;

pub type CompletionResult<'a> =
    Pin<Box<dyn Future<Output = Result<String, Box<dyn Error + Send + Sync>>> + Send + 'a>>;

pub trait ModelBackend: Send + Sync {
    fn complete(&self, prompt: String) -> CompletionResult<'_>;

    fn complete_chat(&self, messages: Vec<Message>) -> CompletionResult<'_>;
}
