use async_trait::async_trait;

#[async_trait]
pub trait Service: Send + Sync {
    async fn run(&self);
}
