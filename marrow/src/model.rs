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

pub type EmbedResult<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<Vec<f32>>, Box<dyn Error + Send + Sync>>> + Send + 'a>>;

pub trait EmbedBackend: Send + Sync {
    fn embed(&self, texts: Vec<String>) -> EmbedResult<'_>;
}
