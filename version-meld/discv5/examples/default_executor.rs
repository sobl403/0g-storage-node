//! Demonstrates how to run a basic Discovery v5 Service with the default Tokio executor.
//!
//! Discv5 requires a Tokio executor with all features. If none is passed, it will use the current
//! runtime that built the `Discv5` struct.
//!
//! To run this example simply run:
//! ```
//! $ cargo run --example default_executor <BASE64ENR>
//! ```

use discv5::{enr, enr::CombinedKey, Discv5, Discv5ConfigBuilder, Discv5Event};
use std::net::SocketAddr;
use tokio::runtime::Runtime;

#[tokio::main]
async fn main() {
    // allows detailed logging with the RUST_LOG env variable
    let filter_layer = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new("info"))
        .unwrap();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter_layer)
        .try_init();

    // listening address and port
    let listen_addr = "0.0.0.0:9000".parse::<SocketAddr>().unwrap();

    let enr_key = CombinedKey::generate_secp256k1();
    // construct a local ENR
    let enr = enr::EnrBuilder::new("v4").build(&enr_key).unwrap();

    // default configuration - uses the current executor
    let config = Discv5ConfigBuilder::new().build();

    // construct the discv5 server
    let mut discv5 = Discv5::new(enr, enr_key, config).unwrap();

    // if we know of another peer's ENR, add it known peers
    if let Some(base64_enr) = std::env::args().nth(1) {
        match base64_enr.parse::<enr::Enr<enr::CombinedKey>>() {
            Ok(enr) => {
                println!(
                    "ENR Read. ip: {:?}, udp_port {:?}, tcp_port: {:?}",
                    enr.ip(),
                    enr.udp(),
                    enr.tcp()
                );
                if let Err(e) = discv5.add_enr(enr) {
                    println!("ENR was not added: {}", e);
                }
            }
            Err(e) => panic!("Decoding ENR failed: {}", e),
        }
    }

    // start the discv5 service
    discv5.start(listen_addr).await.unwrap();
    println!("Server started");

    // get an event stream
    let mut event_stream = discv5.event_stream().await.unwrap();

    loop {
        match event_stream.recv().await {
            Some(Discv5Event::SocketUpdated(addr)) => {
                println!("Nodes ENR socket address has been updated to: {:?}", addr);
            }
            Some(Discv5Event::Discovered(enr)) => {
                println!("A peer has been discovered: {}", enr.node_id());
            }
            _ => {}
        }
    }
}
