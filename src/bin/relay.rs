use moblink_rust::relay;
use std::time::Duration;
use tokio::{fs::File, io::AsyncReadExt, process::Command};

use clap::Parser;
use log::{error, info, warn};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use relay::GetStatusClosure;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Name to identify the relay
    #[arg(short, long, default_value = "Relay")]
    name: String,

    /// Relay ID (valid UUID)
    #[arg(short, long)]
    id: Option<String>,

    /// Streamer URL (websocket) - optional if using mDNS
    #[arg(short = 'u', long)]
    streamer_url: Option<String>,

    /// Password
    #[arg(short, long)]
    password: String,

    /// Bind address
    #[arg(short, long = "bind-address", default_value_t = String::new())]
    bind_address: String,

    /// Log level
    #[arg(short, long, default_value = "info")]
    log_level: String,

    /// Status executable.
    /// Print status to standard output on format {"batteryPercentage": 93}.
    #[arg(long)]
    status_executable: Option<String>,

    /// Status file.
    /// Contains status on format {"batteryPercentage": 93}.
    #[arg(long)]
    status_file: Option<String>,
}

fn setup_logging(log_level: &str) {
    env_logger::builder()
        .default_format()
        .format_timestamp_millis()
        .parse_filters(log_level)
        .init();
}

fn create_get_status_closure(
    status_executable: &Option<String>,
    status_file: &Option<String>,
) -> GetStatusClosure {
    let status_executable = status_executable.clone();
    let status_file = status_file.clone();
    Box::new(move || {
        let status_executable = status_executable.clone();
        let status_file = status_file.clone();
        Box::pin(async move {
            let output = if let Some(status_executable) = &status_executable {
                let Ok(output) = Command::new(status_executable).output().await else {
                    return Default::default();
                };
                output.stdout
            } else if let Some(status_file) = &status_file {
                let Ok(mut file) = File::open(status_file).await else {
                    return Default::default();
                };
                let mut contents = vec![];
                if file.read_to_end(&mut contents).await.is_err() {
                    return Default::default();
                }
                contents
            } else {
                return Default::default();
            };
            let output = String::from_utf8(output).unwrap_or_default();
            match serde_json::from_str(&output) {
                Ok(status) => status,
                Err(e) => {
                    error!("Failed to decode status with error: {e}");
                    Default::default()
                }
            }
        })
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    setup_logging(&args.log_level);
    let relay_id = args.id.clone().unwrap_or(uuid::Uuid::new_v4().to_string());

    if let Some(streamer_url) = args.streamer_url.clone() {
        run_manual(args, relay_id, streamer_url).await;
    } else {
        run_automatic(args, relay_id).await;
    }

    Ok(())
}

async fn run_manual(args: Args, relay_id: String, streamer_url: String) {
    let relay = relay::Relay::new();

    if !args.bind_address.is_empty() {
        relay.lock().await.set_bind_address(args.bind_address);
    }

    relay
        .lock()
        .await
        .setup(
            streamer_url,
            args.password,
            relay_id,
            args.name,
            |status| info!("Status: {}", status),
            create_get_status_closure(&args.status_executable, &args.status_file),
        )
        .await;
    relay.lock().await.start().await;

    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

async fn run_automatic(args: Args, relay_id: String) {
    let mdns_task = tokio::spawn(async move {
        let mut retries = 0;

        loop {
            let mdns = ServiceDaemon::new().expect("Failed to create mDNS daemon");
            let receiver = mdns
                .browse("_moblink._tcp.local.")
                .expect("Failed to browse services");

            info!("Searching for Moblink streamers via mDNS...");
            let relay = relay::Relay::new();

            while let Ok(event) = receiver.recv_async().await {
                let mut relay = relay.lock().await;
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        if relay.is_started() {
                            warn!("Relay already started, skipping discovery");
                            continue;
                        }
                        // Handle network interface binding
                        if !args.bind_address.is_empty() {
                            relay.set_bind_address(args.bind_address.clone());
                        }

                        let port = info.get_port();
                        for ip in info.get_addresses() {
                            if ip.is_loopback() || ip.is_multicast() {
                                continue;
                            }

                            // Skip IPv6 for now
                            if ip.is_ipv6() {
                                continue;
                            }

                            let streamer_url = format!("ws://{}:{}", ip, port);
                            info!("Discovered Moblink streamer at {}", streamer_url);

                            relay
                                .setup(
                                    streamer_url,
                                    args.password.clone(),
                                    relay_id.clone(),
                                    args.name.clone(),
                                    |status| info!("Status: {}", status),
                                    create_get_status_closure(
                                        &args.status_executable,
                                        &args.status_file,
                                    ),
                                )
                                .await;
                        }

                        relay.start().await;
                    }
                    ServiceEvent::ServiceRemoved(_, _) => {
                        warn!("Streamer service removed");
                        relay.stop().await;
                    }
                    _ => {}
                }
            }

            // Reconnect logic with backoff
            let delay = Duration::from_secs(2u64.pow(retries));
            warn!("No streamers found, retrying in {:?}...", delay);
            tokio::time::sleep(delay).await;
            retries = (retries + 1).min(5);
        }
    });

    if let Err(e) = mdns_task.await {
        warn!("mDNS task failed: {:?}", e);
    }
}
