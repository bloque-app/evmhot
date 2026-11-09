use async_trait::async_trait;

// async_trait's expansion trips clippy::double_must_use on newer toolchains.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait Service: Send + Sync {
    async fn run(&self);
}
