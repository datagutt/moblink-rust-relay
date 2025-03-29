use serde::Deserialize;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, Weak};

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout, Duration};
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::protocol::*;

// Move to other file.
pub const MDNS_SERVICE_TYPE: &str = "_moblink._tcp.local.";

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub battery_percentage: Option<i32>,
}

pub type GetStatusClosure =
    Box<dyn Fn() -> Pin<Box<dyn Future<Output = Status> + Send + Sync>> + Send + Sync>;

pub struct Relay {
    me: Weak<Mutex<Self>>,
    /// Store a local IP address  for binding UDP sockets
    bind_address: String,
    relay_id: String,
    streamer_url: String,
    password: String,
    name: String,
    on_status_updated: Option<Box<dyn Fn(String) + Send + Sync>>,
    get_status: Option<Arc<GetStatusClosure>>,
    ws_writer: Option<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>,
    started: bool,
    connected: bool,
    wrong_password: bool,
    reconnect_on_tunnel_error: Arc<Mutex<bool>>,
    start_on_reconnect_soon: Arc<Mutex<bool>>,
}

impl Relay {
    pub fn new() -> Arc<Mutex<Self>> {
        Arc::new_cyclic(|me| {
            Mutex::new(Self {
                me: me.clone(),
                bind_address: Self::get_default_bind_address(),
                relay_id: "".to_string(),
                streamer_url: "".to_string(),
                password: "".to_string(),
                name: "".to_string(),
                on_status_updated: None,
                get_status: None,
                ws_writer: None,
                started: false,
                connected: false,
                wrong_password: false,
                reconnect_on_tunnel_error: Arc::new(Mutex::new(false)),
                start_on_reconnect_soon: Arc::new(Mutex::new(false)),
            })
        })
    }

    pub fn set_bind_address(&mut self, address: String) {
        self.bind_address = address;
    }

    pub async fn setup<F>(
        &mut self,
        streamer_url: String,
        password: String,
        relay_id: String,
        name: String,
        on_status_updated: F,
        get_status: GetStatusClosure,
    ) where
        F: Fn(String) + Send + Sync + 'static,
    {
        self.on_status_updated = Some(Box::new(on_status_updated));
        self.get_status = Some(Arc::new(get_status));
        self.relay_id = relay_id;
        self.streamer_url = streamer_url;
        self.password = password;
        self.name = name;
        info!("Binding to address: {:?}", self.bind_address);
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    pub async fn start(&mut self) {
        if !self.started {
            self.started = true;
            self.start_internal().await;
        }
    }

    pub async fn stop(&mut self) {
        if self.started {
            self.started = false;
            self.stop_internal().await;
        }
    }

    fn get_default_bind_address() -> String {
        // Get main network interface
        let interfaces = pnet::datalink::interfaces();
        let interface = interfaces.iter().find(|interface| {
            interface.is_up() && !interface.is_loopback() && !interface.ips.is_empty()
        });

        // Only ipv4 addresses are supported
        let ipv4_addresses: Vec<String> = interface
            .expect("No available network interfaces found")
            .ips
            .iter()
            .filter_map(|ip| {
                let ip = ip.ip();
                ip.is_ipv4().then(|| ip.to_string())
            })
            .collect();

        // Return the first address
        ipv4_addresses
            .first()
            .cloned()
            .unwrap_or("0.0.0.0:0".to_string())
    }

    async fn start_internal(&mut self) {
        info!("Start internal");
        if !self.started {
            self.stop_internal().await;
            return;
        }

        let request = match url::Url::parse(&self.streamer_url) {
            Ok(url) => url,
            Err(e) => {
                error!("Failed to parse URL: {}", e);
                return;
            }
        };

        match timeout(Duration::from_secs(10), connect_async(request.to_string())).await {
            Ok(Ok((ws_stream, _))) => {
                info!("WebSocket connected");
                let (writer, reader) = ws_stream.split();
                self.ws_writer = Some(writer);
                self.start_websocket_receiver(reader);
            }
            Ok(Err(error)) => {
                // This means the future completed but the connection failed
                error!("WebSocket connection failed immediately: {}", error);
                self.reconnect_soon().await;
            }
            Err(_elapsed) => {
                // This means the future did NOT complete within 10 seconds
                error!("WebSocket connection attempt timed out after 10 seconds");
                self.reconnect_soon().await;
            }
        }
    }

    fn start_websocket_receiver(
        &mut self,
        mut reader: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    ) {
        // Task to process messages received from the channel.
        let relay = self.me.clone();

        tokio::spawn(async move {
            let Some(relay_arc) = relay.upgrade() else {
                return;
            };

            while let Some(result) = reader.next().await {
                let mut relay = relay_arc.lock().await;
                match result {
                    Ok(message) => match message {
                        Message::Text(text) => {
                            if let Ok(message) = serde_json::from_str::<MessageToRelay>(&text) {
                                relay.handle_message(message).await.ok();
                            } else {
                                error!("Failed to deserialize message: {}", text);
                            }
                        }
                        Message::Binary(data) => {
                            debug!("Received binary message of length: {}", data.len());
                        }
                        Message::Ping(data) => {
                            relay.send_message(Message::Pong(data)).await.ok();
                        }
                        Message::Pong(_) => {
                            debug!("Received pong message");
                        }
                        Message::Close(frame) => {
                            info!("Received close message: {:?}", frame);
                            relay.reconnect_soon().await;
                            break;
                        }
                        Message::Frame(_) => {
                            unreachable!("This is never used")
                        }
                    },
                    Err(e) => {
                        error!("Error processing message: {}", e);
                        // TODO: There has to be a better way to handle this
                        if e.to_string()
                            .contains("Connection reset without closing handshake")
                        {
                            relay.reconnect_soon().await;
                        }
                        break;
                    }
                }
            }
        });
    }

    async fn stop_internal(&mut self) {
        info!("Stop internal");
        if let Some(mut ws_writer) = self.ws_writer.take() {
            if let Err(e) = ws_writer.close().await {
                error!("Error closing WebSocket: {}", e);
            } else {
                info!("WebSocket closed successfully");
            }
        }
        self.connected = false;
        self.wrong_password = false;
        *self.reconnect_on_tunnel_error.lock().await = false;
        *self.start_on_reconnect_soon.lock().await = false;
        self.update_status();
    }

    fn update_status(&self) {
        let Some(on_status_updated) = &self.on_status_updated else {
            return;
        };
        let status = if self.connected {
            "Connected to streamer"
        } else if self.wrong_password {
            "Wrong password"
        } else if self.started {
            "Connecting to streamer"
        } else {
            "Disconnected from streamer"
        };
        on_status_updated(status.to_string());
    }

    async fn reconnect_soon(&mut self) {
        self.stop_internal().await;
        *self.start_on_reconnect_soon.lock().await = false;
        let start_on_reconnect_soon = Arc::new(Mutex::new(true));
        self.start_on_reconnect_soon = start_on_reconnect_soon.clone();
        self.start_soon(start_on_reconnect_soon);
    }

    fn start_soon(&mut self, start_on_reconnect_soon: Arc<Mutex<bool>>) {
        let relay = self.me.clone();

        tokio::spawn(async move {
            info!("Reconnecting in 5 seconds...");
            sleep(Duration::from_secs(5)).await;

            if *start_on_reconnect_soon.lock().await {
                info!("Reconnecting...");
                if let Some(relay) = relay.upgrade() {
                    relay.lock().await.start_internal().await;
                }
            }
        });
    }

    async fn handle_message(
        &mut self,
        message: MessageToRelay,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match message {
            MessageToRelay::Hello(hello) => self.handle_message_hello(hello).await,
            MessageToRelay::Identified(identified) => {
                self.handle_message_identified(identified).await
            }
            MessageToRelay::Request(request) => self.handle_message_request(request).await,
        }
    }

    async fn handle_message_hello(
        &mut self,
        hello: Hello,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let authentication = calculate_authentication(
            &self.password,
            &hello.authentication.salt,
            &hello.authentication.challenge,
        );
        let identify = Identify {
            id: self.relay_id.clone(),
            name: self.name.clone(),
            authentication,
        };
        self.send(MessageToStreamer::Identify(identify)).await
    }

    async fn handle_message_identified(
        &mut self,
        identified: Identified,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match identified.result {
            MoblinkResult::Ok(_) => {
                self.connected = true;
            }
            MoblinkResult::WrongPassword(_) => {
                self.wrong_password = true;
            }
        }
        self.update_status();
        Ok(())
    }

    async fn handle_message_request(
        &mut self,
        request: MessageRequest,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match &request.data {
            MessageRequestData::StartTunnel(start_tunnel) => {
                self.handle_message_request_start_tunnel(&request, start_tunnel)
                    .await
            }
            MessageRequestData::Status(_) => self.handle_message_request_status(request).await,
        }
    }

    async fn handle_message_request_start_tunnel(
        &mut self,
        request: &MessageRequest,
        start_tunnel: &StartTunnelRequest,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Pick bind addresses from the relay
        let local_bind_addr_for_streamer = parse_socket_addr("0.0.0.0")?;
        let local_bind_addr_for_destination = parse_socket_addr(&self.bind_address)?;

        info!(
            "Binding streamer socket on: {}, destination socket on: {}",
            local_bind_addr_for_streamer, local_bind_addr_for_destination
        );
        // Create a UDP socket bound for receiving packets from the server.
        // Use dual-stack socket creation.
        let streamer_socket = create_dual_stack_udp_socket(local_bind_addr_for_streamer).await?;
        let streamer_port = streamer_socket.local_addr()?.port();
        info!("Listening on UDP port: {}", streamer_port);
        let streamer_socket = Arc::new(streamer_socket);

        // Inform the server about the chosen port.
        let data = ResponseData::StartTunnel(StartTunnelResponseData {
            port: streamer_port,
        });
        let response = request.to_ok_response(data);
        self.send(MessageToStreamer::Response(response)).await?;

        // Create a new UDP socket for communication with the destination.
        // Use dual-stack socket creation.
        let destination_socket =
            create_dual_stack_udp_socket(local_bind_addr_for_destination).await?;

        info!(
            "Bound destination socket to: {:?}",
            destination_socket.local_addr()?
        );
        let destination_socket = Arc::new(destination_socket);

        let normalized_ip = match IpAddr::from_str(&start_tunnel.address)? {
            IpAddr::V4(v4) => IpAddr::V4(v4),
            IpAddr::V6(v6) => {
                // If it’s an IPv4-mapped IPv6 like ::ffff:x.x.x.x, convert to real IPv4
                if let Some(mapped_v4) = v6.to_ipv4() {
                    IpAddr::V4(mapped_v4)
                } else {
                    // Otherwise, keep it as IPv6
                    IpAddr::V6(v6)
                }
            }
        };
        let destination_addr = SocketAddr::new(normalized_ip, start_tunnel.port);
        info!("Destination address resolved: {}", destination_addr);

        // Use an Arc<Mutex> to share the server_remote_addr between tasks.
        let streamer_addr: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));

        let relay_to_destination = start_relay_from_streamer_to_destination(
            streamer_socket.clone(),
            destination_socket.clone(),
            streamer_addr.clone(),
            destination_addr,
        );
        let relay_to_streamer = start_relay_from_destination_to_streamer(
            streamer_socket,
            destination_socket,
            streamer_addr,
        );

        *self.reconnect_on_tunnel_error.lock().await = false;
        let reconnect_on_tunnel_error = Arc::new(Mutex::new(true));
        self.reconnect_on_tunnel_error = reconnect_on_tunnel_error.clone();
        let relay = self.me.clone();

        tokio::spawn(async move {
            let Some(relay) = relay.upgrade() else {
                return;
            };

            // Wait for relay tasks to complete (they won't unless an error occurs or the
            // socket is closed).
            tokio::select! {
                res = relay_to_destination => {
                    if let Err(e) = res {
                        error!("relay_to_destination task failed: {}", e);
                    }
                }
                res = relay_to_streamer => {
                    if let Err(e) = res {
                        error!("relay_to_streamer task failed: {}", e);
                    }
                }
            }

            if *reconnect_on_tunnel_error.lock().await {
                relay.lock().await.reconnect_soon().await;
            } else {
                info!("Not reconnecting after tunnel error");
            }
        });

        Ok(())
    }

    async fn handle_message_request_status(
        &mut self,
        request: MessageRequest,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Some(get_status) = self.get_status.as_ref() else {
            error!("get_battery_percentage is not set");
            return Err("get_battery_percentage function not set".into());
        };
        let status = get_status().await;
        let data = ResponseData::Status(StatusResponseData {
            battery_percentage: status.battery_percentage,
        });
        let response = request.to_ok_response(data);
        self.send(MessageToStreamer::Response(response)).await
    }

    async fn send(
        &mut self,
        message: MessageToStreamer,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let text = serde_json::to_string(&message)?;
        self.send_message(Message::Text(text.into())).await
    }

    async fn send_message(
        &mut self,
        message: Message,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Some(writer) = self.ws_writer.as_mut() else {
            return Err("No websocket writer".into());
        };
        writer.send(message).await?;
        Ok(())
    }
}

fn start_relay_from_streamer_to_destination(
    streamer_socket: Arc<UdpSocket>,
    destination_socket: Arc<UdpSocket>,
    streamer_addr: Arc<Mutex<Option<SocketAddr>>>,
    destination_addr: SocketAddr,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        debug!("(relay_to_destination) Task started");
        loop {
            let mut buf = [0; 2048];
            let (size, remote_addr) =
                match timeout(Duration::from_secs(30), streamer_socket.recv_from(&mut buf)).await {
                    Ok(result) => match result {
                        Ok((size, addr)) => (size, addr),
                        Err(e) => {
                            error!("(relay_to_destination) Error receiving from server: {}", e);
                            continue;
                        }
                    },
                    Err(e) => {
                        error!(
                            "(relay_to_destination) Timeout receiving from server: {}",
                            e
                        );
                        break;
                    }
                };

            debug!(
                "(relay_to_destination) Received {} bytes from server: {}",
                size, remote_addr
            );

            // Forward to destination.
            match destination_socket
                .send_to(&buf[..size], &destination_addr)
                .await
            {
                Ok(bytes_sent) => {
                    debug!(
                        "(relay_to_destination) Sent {} bytes to destination",
                        bytes_sent
                    )
                }
                Err(e) => {
                    error!(
                        "(relay_to_destination) Failed to send to destination: {}",
                        e
                    );
                    break;
                }
            }

            // Set the remote address if it hasn't been set yet.
            let mut streamer_addr_lock = streamer_addr.lock().await;
            if streamer_addr_lock.is_none() {
                *streamer_addr_lock = Some(remote_addr);
                debug!(
                    "(relay_to_destination) Server remote address set to: {}",
                    remote_addr
                );
            }
        }
        info!("(relay_to_destination) Task exiting");
    })
}

fn start_relay_from_destination_to_streamer(
    streamer_socket: Arc<UdpSocket>,
    destination_socket: Arc<UdpSocket>,
    streamer_addr: Arc<Mutex<Option<SocketAddr>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        debug!("(relay_to_streamer) Task started");
        loop {
            let mut buf = [0; 2048];
            let (size, remote_addr) = match timeout(
                Duration::from_secs(30),
                destination_socket.recv_from(&mut buf),
            )
            .await
            {
                Ok(result) => match result {
                    Ok((size, addr)) => (size, addr),
                    Err(e) => {
                        error!(
                            "(relay_to_streamer) Error receiving from destination: {}",
                            e
                        );
                        continue;
                    }
                },
                Err(e) => {
                    error!(
                        "(relay_to_streamer) Timeout receiving from destination: {}",
                        e
                    );
                    break;
                }
            };

            debug!(
                "(relay_to_streamer) Received {} bytes from destination: {}",
                size, remote_addr
            );
            // Forward to server.
            let streamer_addr_lock = streamer_addr.lock().await;
            match *streamer_addr_lock {
                Some(streamer_addr) => {
                    match streamer_socket.send_to(&buf[..size], &streamer_addr).await {
                        Ok(bytes_sent) => {
                            debug!("(relay_to_streamer) Sent {} bytes to server", bytes_sent)
                        }
                        Err(e) => {
                            error!("(relay_to_streamer) Failed to send to server: {}", e);
                            break;
                        }
                    }
                }
                None => {
                    error!("(relay_to_streamer) Server address not set, cannot forward packet");
                }
            }
        }
        info!("(relay_to_streamer) Task exiting");
    })
}

async fn create_dual_stack_udp_socket(
    addr: SocketAddr,
) -> Result<tokio::net::UdpSocket, std::io::Error> {
    let socket = match addr.is_ipv4() {
        true => {
            // Create an IPv4 socket
            tokio::net::UdpSocket::bind(addr).await?
        }
        false => {
            // Create a dual-stack socket (supporting both IPv4 and IPv6)
            let socket = socket2::Socket::new(
                socket2::Domain::IPV6,
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            )?;

            // Set IPV6_V6ONLY to false to enable dual-stack support
            socket.set_only_v6(false)?;

            // Bind the socket
            socket.bind(&socket2::SockAddr::from(addr))?;

            // Convert to a tokio UdpSocket
            tokio::net::UdpSocket::from_std(socket.into())?
        }
    };

    Ok(socket)
}

// Helper function to parse a string into a SocketAddr, handling IP addresses
// without ports.
fn parse_socket_addr(addr_str: &str) -> Result<SocketAddr, std::io::Error> {
    // Attempt to parse the string as a full SocketAddr (IP:port)
    if let Ok(socket_addr) = SocketAddr::from_str(addr_str) {
        return Ok(socket_addr);
    }

    // If parsing as SocketAddr fails, try parsing as IP address and append default
    // port
    if let Ok(ip_addr) = IpAddr::from_str(addr_str) {
        // Use 0 as the default port, allowing the OS to assign an available port
        return Ok(SocketAddr::new(ip_addr, 0));
    }

    // Return an error if both attempts fail
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "Invalid socket address syntax. Expected 'IP:port' or 'IP'.",
    ))
}
