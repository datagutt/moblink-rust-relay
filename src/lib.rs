mod protocol;
pub mod relay;
pub use relay::*;
pub mod streamer;
pub use streamer::*;

pub const MDNS_SERVICE_TYPE: &str = "_moblink._tcp.local.";
