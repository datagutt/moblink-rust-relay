use crate::protocol::{
    calculate_authentication, Authentication, Hello, Identified, Identify, MessageRequest,
    MessageRequestData, MessageResponse, MessageToRelay, MessageToStreamer, MoblinkResult, Present,
    ResponseData, StartTunnelRequest, API_VERSION,
};
use crate::MDNS_SERVICE_TYPE;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use log::{error, info};
use rand::distr::{Alphanumeric, SampleString};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use mdns_sd::{ServiceDaemon, ServiceInfo};

pub type TunnelCreatedClosure = Box<
    dyn Fn(String, String, String, u16) -> Pin<Box<dyn Future<Output = ()> + Send + Sync>>
        + Send
        + Sync,
>;

pub type TunnelDestroyedClosure = Box<
    dyn Fn(String, String, String, u16) -> Pin<Box<dyn Future<Output = ()> + Send + Sync>>
        + Send
        + Sync,
>;

struct Relay {
    me: Weak<Mutex<Self>>,
    streamer: Weak<Mutex<Streamer>>,
    remote_address: SocketAddr,
    writer: SplitSink<WebSocketStream<TcpStream>, Message>,
    challenge: String,
    salt: String,
    identified: bool,
    relay_id: String,
    relay_name: String,
    tunnel_port: Option<u16>,
}

impl Relay {
    pub fn new(
        streamer: Weak<Mutex<Streamer>>,
        remote_address: SocketAddr,
        writer: SplitSink<WebSocketStream<TcpStream>, Message>,
    ) -> Arc<Mutex<Self>> {
        Arc::new_cyclic(|me| {
            Mutex::new(Self {
                me: me.clone(),
                streamer,
                remote_address,
                writer,
                challenge: String::new(),
                salt: String::new(),
                identified: false,
                relay_id: "".into(),
                relay_name: "".into(),
                tunnel_port: None,
            })
        })
    }

    fn start(&mut self, mut reader: SplitStream<WebSocketStream<TcpStream>>) {
        let relay = self.me.clone();

        tokio::spawn(async move {
            let Some(relay) = relay.upgrade() else {
                return;
            };

            relay.lock().await.start_handshake().await;

            while let Some(Ok(message)) = reader.next().await {
                if let Err(error) = relay.lock().await.handle_websocket_message(message).await {
                    error!("Relay error: {}", error);
                    break;
                }
            }

            let mut relay_guard = relay.lock().await;
            info!("Relay disconnected: {}", relay_guard.remote_address);
            relay_guard.tunnel_destroyed().await;
            if let Some(streamer) = relay_guard.streamer.upgrade() {
                streamer.lock().await.remove_relay(&relay);
            }
        });
    }

    async fn handle_websocket_message(
        &mut self,
        message: Message,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match message {
            Message::Text(text) => match serde_json::from_str(&text) {
                Ok(message) => self.handle_message(message).await,
                Err(error) => {
                    Err(format!("Failed to deserialize message with error: {}", error).into())
                }
            },
            Message::Ping(data) => Ok(self.writer.send(Message::Pong(data)).await?),
            _ => Err(format!("Unsupported websocket message: {:?}", message).into()),
        }
    }

    async fn handle_message(
        &mut self,
        message: MessageToStreamer,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match message {
            MessageToStreamer::Identify(identify) => self.handle_message_identify(identify).await,
            MessageToStreamer::Response(response) => self.handle_message_response(response).await,
        }
    }

    async fn handle_message_identify(
        &mut self,
        identify: Identify,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Some(streamer) = self.streamer.upgrade() else {
            return Err("No streamer".into());
        };
        if identify.authentication
            == calculate_authentication(
                &streamer.lock().await.password,
                &self.salt,
                &self.challenge,
            )
        {
            self.identified = true;
            self.relay_id = identify.id;
            self.relay_name = identify.name;
            let identified = Identified {
                result: MoblinkResult::Ok(Present {}),
            };
            self.send(MessageToRelay::Identified(identified)).await?;
            self.start_tunnel().await
        } else {
            let identified = Identified {
                result: MoblinkResult::WrongPassword(Present {}),
            };
            self.send(MessageToRelay::Identified(identified)).await?;
            Err("Relay sent wrong password".into())
        }
    }

    async fn handle_message_response(
        &mut self,
        response: MessageResponse,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match response.data {
            ResponseData::StartTunnel(data) => {
                self.tunnel_port = Some(data.port);
                self.tunnel_created().await;
            }
            message => {
                info!("Ignoring message {:?}", message);
            }
        }
        Ok(())
    }

    async fn tunnel_created(&self) {
        let Some(tunnel_port) = self.tunnel_port else {
            return;
        };
        info!(
            "Tunnel created: {}:{} ({}, {})",
            self.remote_address.ip(),
            tunnel_port,
            self.relay_name,
            self.relay_id
        );
        let Some(streamer) = self.streamer.upgrade() else {
            return;
        };
        let Some(tunnel_created) = &streamer.lock().await.tunnel_created else {
            return;
        };
        tunnel_created(
            self.relay_id.clone(),
            self.relay_name.clone(),
            self.remote_address.ip().to_string(),
            tunnel_port,
        )
        .await;
    }

    async fn tunnel_destroyed(&mut self) {
        let Some(tunnel_port) = self.tunnel_port.take() else {
            return;
        };
        info!(
            "Tunnel destroyed: {}:{} ({}, {})",
            self.remote_address.ip(),
            tunnel_port,
            self.relay_name,
            self.relay_id
        );
        let Some(streamer) = self.streamer.upgrade() else {
            return;
        };
        let Some(tunnel_destroyed) = &streamer.lock().await.tunnel_destroyed else {
            return;
        };
        tunnel_destroyed(
            self.relay_id.clone(),
            self.relay_name.clone(),
            self.remote_address.ip().to_string(),
            tunnel_port,
        )
        .await;
    }

    async fn start_handshake(&mut self) {
        self.challenge = random_string();
        self.salt = random_string();
        self.send_hello().await;
        self.identified = false;
    }

    async fn start_tunnel(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Some(streamer) = self.streamer.upgrade() else {
            return Err("No streamer".into());
        };
        let streamer = streamer.lock().await;
        let start_tunnel = StartTunnelRequest {
            address: streamer.destination_address.clone(),
            port: streamer.destination_port,
        };
        let request = MessageRequest {
            id: 1,
            data: MessageRequestData::StartTunnel(start_tunnel),
        };
        self.send(MessageToRelay::Request(request)).await
    }

    async fn send_hello(&mut self) {
        let hello = MessageToRelay::Hello(Hello {
            api_version: API_VERSION.into(),
            authentication: Authentication {
                challenge: self.challenge.clone(),
                salt: self.salt.clone(),
            },
        });
        self.send(hello).await.ok();
    }

    async fn send(
        &mut self,
        message: MessageToRelay,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let text = serde_json::to_string(&message)?;
        self.writer.send(Message::Text(text.into())).await?;
        Ok(())
    }
}

pub struct Streamer {
    me: Weak<Mutex<Self>>,
    id: String,
    name: String,
    address: String,
    port: u16,
    password: String,
    destination_address: String,
    destination_port: u16,
    tunnel_created: Option<TunnelCreatedClosure>,
    tunnel_destroyed: Option<TunnelDestroyedClosure>,
    relays: Vec<Arc<Mutex<Relay>>>,
}

impl Streamer {
    pub fn new(
        id: String,
        name: String,
        address: String,
        port: u16,
        password: String,
        destination_address: String,
        destination_port: u16,
        tunnel_created: Option<TunnelCreatedClosure>,
        tunnel_destroyed: Option<TunnelDestroyedClosure>,
    ) -> Arc<Mutex<Self>> {
        Arc::new_cyclic(|me| {
            Mutex::new(Self {
                me: me.clone(),
                id,
                name,
                address,
                port,
                password,
                destination_address,
                destination_port,
                tunnel_created,
                tunnel_destroyed,
                relays: Vec::new(),
            })
        })
    }

    pub async fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let listener_address = format!("{}:{}", self.address, self.port);
        let listener = TcpListener::bind(&listener_address).await?;

        info!("WebSocket server listening on '{}'", listener_address);

        let (service_daemon, service_info) = self.create_mdns_service()?;

        let streamer = self.me.clone();

        tokio::spawn(async move {
            if let Err(error) = service_daemon.register(service_info) {
                error!("Failed to register mDNS service with error: {}", error);
            }

            while let Ok((tcp_stream, remote_address)) = listener.accept().await {
                if let Some(streamer) = streamer.upgrade() {
                    streamer
                        .lock()
                        .await
                        .handle_relay_connection(tcp_stream, remote_address)
                        .await;
                } else {
                    break;
                }
            }
        });

        Ok(())
    }

    fn create_mdns_service(
        &self,
    ) -> Result<(ServiceDaemon, ServiceInfo), Box<dyn std::error::Error>> {
        let service_daemon = ServiceDaemon::new()?;
        let properties = HashMap::from([("name".to_string(), self.name.clone())]);
        let service_info = ServiceInfo::new(
            MDNS_SERVICE_TYPE,
            &self.id,
            &format!("{}.local.", self.id),
            &self.address,
            self.port,
            properties,
        )?;
        Ok((service_daemon, service_info))
    }

    async fn handle_relay_connection(&mut self, tcp_stream: TcpStream, remote_address: SocketAddr) {
        match tokio_tungstenite::accept_async(tcp_stream).await {
            Ok(websocket_stream) => {
                info!("Relay connected: {}", remote_address);
                let (writer, reader) = websocket_stream.split();
                let relay = Relay::new(self.me.clone(), remote_address, writer);
                relay.lock().await.start(reader);
                self.add_relay(relay);
            }
            Err(error) => {
                error!("Relay websocket handshake failed with: {}", error);
            }
        }
    }

    fn add_relay(&mut self, relay: Arc<Mutex<Relay>>) {
        self.relays.push(relay);
        self.log_number_of_relays();
    }

    fn remove_relay(&mut self, relay: &Arc<Mutex<Relay>>) {
        self.relays.retain(|r| !Arc::ptr_eq(r, relay));
        self.log_number_of_relays();
    }

    fn log_number_of_relays(&self) {
        info!("Number of relays: {}", self.relays.len())
    }
}

fn random_string() -> String {
    Alphanumeric.sample_string(&mut rand::rng(), 64)
}
