//! wallermax-server: a modular, secure and high-performance web server.
//!
//! This binary is a thin wrapper: all the interesting logic lives in the
//! library crate (`src/lib.rs` and its modules) so that it can be reused,
//! embedded and tested independently.

#[tokio::main]
async fn main() {
    if let Err(error) = wallermax_server::run().await {
        eprintln!("wallermax-server failed to start: {error}");
        std::process::exit(1);
    }
}
