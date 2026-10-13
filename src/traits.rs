use async_trait::async_trait;

// async_trait boxes the future, and newer clippy (double_must_use) flags the must_use it adds on top.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait Service: Send + Sync {
    async fn run(&self);
}
