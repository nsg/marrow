use std::error::Error;
use std::future::Future;

use crate::session::Message;
use crate::task::Task;

#[derive(Debug, Clone)]
pub struct Context {
    pub data: serde_json::Value,
}

impl Context {
    pub fn new(data: serde_json::Value) -> Self {
        Self { data }
    }
}

pub trait Executor: Send + Sync {
    fn execute(
        &self,
        task: &Task,
        context: &Context,
        history: Option<&[Message]>,
    ) -> impl Future<Output = Result<serde_json::Value, Box<dyn Error + Send + Sync>>> + Send;
}
