//! Native audio and session contracts shared by the desktop and Android receiver.
pub mod audio;
pub mod control;
pub mod receiver;

pub const SERVICE_TYPE: &str = "_robin._tcp.local.";
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
pub const FRAMES_PER_PACKET: usize = 120;
pub const PACKET_DURATION: std::time::Duration = std::time::Duration::from_micros(2_500);
pub type StereoFrame = (i16, i16);
