use std::time::Duration;

use moblink_rust::{streamer, TunnelCreatedClosure, TunnelDestroyedClosure};

use clap::Parser;
use log::error;
use tokio::process::Command;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Id
    #[arg(long)]
    id: String,

    /// Name
    #[arg(long)]
    name: String,

    /// Password
    #[arg(long)]
    password: String,

    /// Websocket server listener address. Used for mDNS as well right now.
    #[arg(long)]
    websocket_server_address: String,

    /// Websocket server listener port
    #[arg(long)]
    websocket_server_port: u16,

    /// Streaming destination address
    #[arg(long)]
    destination_address: String,

    /// Streaming destination port
    #[arg(long)]
    destination_port: u16,

    /// Tunnel via relay created executable.
    /// Called with --relay-id <id> --relay-name <name> --address <address> --port <port>.
    #[arg(long)]
    tunnel_created: Option<String>,

    /// Tunnel via relay destroyed executable.
    /// Called with --relay-id <id> --relay-name <name> --address <address> --port <port>.
    #[arg(long)]
    tunnel_destroyed: Option<String>,

    /// Log level
    #[arg(long, default_value = "info")]
    log_level: String,
}

fn setup_logging(log_level: &str) {
    env_logger::builder()
        .default_format()
        .format_timestamp_millis()
        .parse_filters(log_level)
        .init();
}

fn create_tunnel_created_closure(executable: Option<String>) -> Option<TunnelCreatedClosure> {
    let executable = executable?;
    Some(Box::new(move |relay_id, relay_name, address, port| {
        let executable = executable.clone();
        Box::pin(async move {
            if let Err(error) = exec(executable, relay_id, relay_name, address, port).await {
                error!("Tunnel created executable failed with: {}", error);
            }
        })
    }))
}

fn create_tunnel_destroyed_closure(executable: Option<String>) -> Option<TunnelDestroyedClosure> {
    let executable = executable?;
    Some(Box::new(move |relay_id, relay_name, address, port| {
        let executable = executable.clone();
        Box::pin(async move {
            if let Err(error) = exec(executable, relay_id, relay_name, address, port).await {
                error!("Tunnel destroyed executable failed with: {}", error);
            }
        })
    }))
}

async fn exec(
    executable: String,
    relay_id: String,
    relay_name: String,
    address: String,
    port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let status = Command::new(executable)
        .arg("--relay-id")
        .arg(relay_id)
        .arg("--relay-name")
        .arg(relay_name)
        .arg("--address")
        .arg(address)
        .arg("--port")
        .arg(port.to_string())
        .status()
        .await?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Failed with status {}", status).into())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    setup_logging(&args.log_level);

    let streamer = streamer::Streamer::new(
        args.id,
        args.name,
        args.websocket_server_address,
        args.websocket_server_port,
        args.password,
        args.destination_address,
        args.destination_port,
        create_tunnel_created_closure(args.tunnel_created),
        create_tunnel_destroyed_closure(args.tunnel_destroyed),
    );
    streamer.lock().await.start().await?;

    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}
