use async_trait::async_trait;
use songbird::{Event, EventContext, EventHandler};
use std::sync::Arc;

pub struct HandlerWrapper(pub Arc<dyn EventHandler + Sync>);

#[async_trait]
impl EventHandler for HandlerWrapper {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        self.0.act(ctx).await
    }
}
