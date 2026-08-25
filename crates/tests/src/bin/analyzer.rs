//! Test-only executable wrapper around the real analyzer library.

fn rpc_port() -> u16 {
    let mut args = std::env::args_os().skip(1);
    let argument = args
        .next()
        .expect("the analyzer test executable requires --rpc-port");
    assert_eq!(argument, "--rpc-port", "unsupported analyzer test argument");
    let port = args
        .next()
        .expect("--rpc-port requires a value")
        .to_string_lossy()
        .parse()
        .expect("--rpc-port must be a valid port");
    assert!(args.next().is_none(), "unexpected analyzer test arguments");
    port
}

#[tokio::main]
async fn main() {
    analyzer::main(Some(rpc_port()), None).await;
}
