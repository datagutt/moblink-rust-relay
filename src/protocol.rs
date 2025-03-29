use base64::engine::general_purpose;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use sha2::Sha256;

pub const API_VERSION: &str = "1.0";

#[derive(Deserialize, Serialize, Debug)]
pub struct Present {}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub enum MoblinkResult {
    Ok(Present),
    WrongPassword(Present),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Authentication {
    pub challenge: String,
    pub salt: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Hello {
    #[serde(rename = "apiVersion")]
    #[allow(dead_code)]
    pub api_version: String,
    pub authentication: Authentication,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Identified {
    pub result: MoblinkResult,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct StartTunnelRequest {
    pub address: String,
    pub port: u16,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub enum MessageRequestData {
    StartTunnel(StartTunnelRequest),
    Status(Present),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct MessageRequest {
    pub id: u32,
    pub data: MessageRequestData,
}

impl MessageRequest {
    pub fn to_ok_response(&self, data: ResponseData) -> MessageResponse {
        MessageResponse {
            id: self.id,
            result: MoblinkResult::Ok(Present {}),
            data,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct StartTunnelResponseData {
    pub port: u16,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StatusResponseData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery_percentage: Option<i32>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub enum ResponseData {
    StartTunnel(StartTunnelResponseData),
    Status(StatusResponseData),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct MessageResponse {
    pub id: u32,
    pub result: MoblinkResult,
    pub data: ResponseData,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Identify {
    pub id: String,
    pub name: String,
    pub authentication: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub enum MessageToRelay {
    Hello(Hello),
    Identified(Identified),
    Request(MessageRequest),
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub enum MessageToStreamer {
    Identify(Identify),
    Response(MessageResponse),
}

pub fn calculate_authentication(password: &str, salt: &str, challenge: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{}{}", password, salt).as_bytes());
    let hash1 = hasher.finalize_reset();
    hasher.update(format!("{}{}", general_purpose::STANDARD.encode(hash1), challenge).as_bytes());
    let hash2 = hasher.finalize();
    general_purpose::STANDARD.encode(hash2)
}
