use env_logger::Env;
use log::{info, trace};

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(Env::default().default_filter_or("trace")).init();
    trace!("test");
    lsp_server::main().await;
}
