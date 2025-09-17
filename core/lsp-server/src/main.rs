#[tokio::main]
async fn main() {
    env_logger::init();
    lsp_server::main().await;
}
