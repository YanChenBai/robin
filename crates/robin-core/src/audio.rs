use crate::{CHANNELS, FRAMES_PER_PACKET, PACKET_DURATION, SAMPLE_RATE, StereoFrame};
use anyhow::{Result, ensure};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use rushaudio::{Packet, PacketType};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

const AUDIO_BYTES: usize = FRAMES_PER_PACKET * CHANNELS as usize * 2;
pub const MAX_DATAGRAM: usize = 1200;

/// Monotonic sender clock; catch up short stalls and skip stale media after long ones.
pub struct PacketPacer {
    deadline: Instant,
    sequence: u64,
}
impl PacketPacer {
    pub fn new(now: Instant) -> Self {
        Self {
            deadline: now,
            sequence: 0,
        }
    }
    pub fn take_due(&mut self, now: Instant) -> Result<Option<u64>> {
        if now < self.deadline {
            return Ok(None);
        }
        let lag = now.duration_since(self.deadline);
        if lag > Duration::from_millis(20) {
            // 序号只统计真正发出的数据包，调度暂停不应伪装成网络丢包。
            self.deadline =
                now - Duration::from_nanos((lag.as_nanos() % PACKET_DURATION.as_nanos()) as u64);
        }
        let sequence = self.sequence;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("audio sequence exhausted"))?;
        self.deadline += PACKET_DURATION;
        Ok(Some(sequence))
    }
}

/// Session-specific authenticated envelope around an unmodified RushAudio packet.
pub struct AudioCipher(ChaCha20Poly1305);

impl AudioCipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self(ChaCha20Poly1305::new(key.into()))
    }

    pub fn seal(&self, sequence: u64, frames: &[StereoFrame]) -> Result<Vec<u8>> {
        ensure!(
            frames.len() == FRAMES_PER_PACKET,
            "invalid PCM packet length"
        );
        let mut pcm = Vec::with_capacity(AUDIO_BYTES);
        for &(left, right) in frames {
            pcm.extend_from_slice(&left.to_le_bytes());
            pcm.extend_from_slice(&right.to_le_bytes());
        }
        let timestamp =
            ((sequence as u128 * FRAMES_PER_PACKET as u128 * 1000) / SAMPLE_RATE as u128) as u32;
        let packet = Packet::new(
            PacketType::AudioData,
            sequence as u32,
            timestamp,
            Packet::encode_audio_payload(CHANNELS, SAMPLE_RATE, 0x02, &pcm),
        )
        .encode();
        let header = sequence.to_be_bytes();
        let mut nonce = [0; 12];
        nonce[4..].copy_from_slice(&header);
        let encrypted = self
            .0
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &packet,
                    aad: &header,
                },
            )
            .map_err(|_| anyhow::anyhow!("audio encryption failed"))?;
        let mut datagram = header.to_vec();
        datagram.extend(encrypted);
        ensure!(
            datagram.len() <= MAX_DATAGRAM,
            "audio datagram exceeds MTU budget"
        );
        Ok(datagram)
    }

    pub fn open(&self, datagram: &[u8]) -> Result<(u64, Vec<StereoFrame>)> {
        ensure!(
            (24..=MAX_DATAGRAM).contains(&datagram.len()),
            "invalid datagram length"
        );
        let header: [u8; 8] = datagram[..8].try_into()?;
        let sequence = u64::from_be_bytes(header);
        let mut nonce = [0; 12];
        nonce[4..].copy_from_slice(&header);
        let plaintext = self
            .0
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &datagram[8..],
                    aad: &header,
                },
            )
            .map_err(|_| anyhow::anyhow!("unauthenticated audio"))?;
        let packet = Packet::decode(&plaintext)?;
        ensure!(
            packet.header.version == 1 && packet.header.packet_type == PacketType::AudioData,
            "unsupported audio packet"
        );
        ensure!(
            packet.total_size() == plaintext.len() && packet.header.sequence == sequence as u32,
            "inconsistent packet header"
        );
        let (channels, rate, codec, pcm) = Packet::decode_audio_payload(&packet.payload)?;
        ensure!(
            channels == CHANNELS
                && rate == SAMPLE_RATE
                && codec == 0x02
                && pcm.len() == AUDIO_BYTES,
            "unsupported audio format"
        );
        let frames = pcm
            .as_chunks::<4>()
            .0
            .iter()
            .map(|s| {
                (
                    i16::from_le_bytes([s[0], s[1]]),
                    i16::from_le_bytes([s[2], s[3]]),
                )
            })
            .collect();
        Ok((sequence, frames))
    }
}

/// Bounded packet reordering. The hardware callback, not this queue, owns the playback clock.
pub struct PlayoutBuffer {
    packets: BTreeMap<u64, Vec<StereoFrame>>,
    next: Option<u64>,
    highest: Option<u64>,
    deadline: Option<Instant>,
    delay: Duration,
    late: u64,
    lost: u64,
    last_frame: StereoFrame,
    fade_next: bool,
}

impl PlayoutBuffer {
    pub fn new(delay_ms: u64) -> Result<Self> {
        ensure!(
            (5..=100).contains(&delay_ms),
            "buffer must be between 5 and 100 ms"
        );
        Ok(Self {
            packets: BTreeMap::new(),
            next: None,
            highest: None,
            deadline: None,
            delay: Duration::from_millis(delay_ms),
            late: 0,
            lost: 0,
            last_frame: (0, 0),
            fade_next: false,
        })
    }
    pub fn push(&mut self, sequence: u64, frames: Vec<StereoFrame>, now: Instant) -> bool {
        if frames.len() != FRAMES_PER_PACKET {
            return false;
        }
        if let Some(next) = self.next {
            if sequence < next {
                self.late += 1;
                return false;
            }
            if sequence.saturating_sub(next) >= 64 {
                // An authenticated newer packet recovers from a long scheduler/network pause.
                self.lost += sequence - next;
                self.packets.clear();
                self.last_frame = (0, 0);
                self.fade_next = true;
                self.next = Some(sequence);
                self.deadline = Some(now + self.delay);
            } else if self.packets.is_empty()
                && self
                    .deadline
                    .is_some_and(|deadline| now.saturating_duration_since(deadline) > self.delay)
            {
                self.deadline = Some(now + self.delay);
            }
        } else {
            self.next = Some(sequence);
            self.deadline = Some(now);
        }
        if self.packets.contains_key(&sequence) {
            return false;
        }
        self.highest = Some(
            self.highest
                .map_or(sequence, |highest| highest.max(sequence)),
        );
        self.packets.insert(sequence, frames);
        true
    }
    pub fn pop_due(&mut self, now: Instant) -> Option<Vec<StereoFrame>> {
        let deadline = self.deadline?;
        let next = self.next?;
        // No newer packet has proved a gap. Silence is a playback underrun, not network loss.
        if next > self.highest? {
            return None;
        }
        match self.packets.remove(&next) {
            Some(mut frame) => {
                if self.fade_next {
                    for (index, sample) in frame.iter_mut().take(48).enumerate() {
                        *sample = (
                            (sample.0 as i32 * index as i32 / 48) as i16,
                            (sample.1 as i32 * index as i32 / 48) as i16,
                        );
                    }
                    self.fade_next = false;
                }
                self.last_frame = *frame.last().unwrap();
                self.next = next.checked_add(1);
                self.deadline = Some(now + self.delay);
                Some(frame)
            }
            None => {
                if now < deadline {
                    return None;
                }
                self.next = next.checked_add(1);
                self.lost += 1;
                // Conceal missing PCM without a full-amplitude jump to silence.
                let mut silence = vec![(0, 0); FRAMES_PER_PACKET];
                for (index, sample) in silence.iter_mut().take(48).enumerate() {
                    let weight = (48 - index) as i32;
                    *sample = (
                        (self.last_frame.0 as i32 * weight / 48) as i16,
                        (self.last_frame.1 as i32 * weight / 48) as i16,
                    );
                }
                self.last_frame = (0, 0);
                self.fade_next = true;
                Some(silence)
            }
        }
    }
    pub fn depth_ms(&self) -> f64 {
        self.packets.len() as f64 * PACKET_DURATION.as_secs_f64() * 1000.0
    }
    pub fn set_delay(&mut self, delay_ms: u64) -> Result<()> {
        ensure!(
            (5..=100).contains(&delay_ms),
            "buffer must be between 5 and 100 ms"
        );
        let delay = Duration::from_millis(delay_ms);
        if let Some(deadline) = self.deadline {
            self.deadline = Some(if delay >= self.delay {
                deadline + (delay - self.delay)
            } else {
                deadline - (self.delay - delay)
            });
        }
        self.delay = delay;
        Ok(())
    }
    pub fn late(&self) -> u64 {
        self.late
    }
    pub fn lost(&self) -> u64 {
        self.lost
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authenticates_rushaudio_and_rejects_wrong_session_and_tampering() {
        let cipher = AudioCipher::new(&[1; 32]);
        let frames = vec![(-100, 200); FRAMES_PER_PACKET];
        let mut bytes = cipher.seal(u32::MAX as u64 + 1, &frames).unwrap();
        assert!(bytes.len() < 1200);
        assert_eq!(bytes.len(), 525);
        assert_eq!(cipher.open(&bytes).unwrap(), (u32::MAX as u64 + 1, frames));
        assert!(AudioCipher::new(&[2; 32]).open(&bytes).is_err());
        bytes[0] ^= 1;
        assert!(cipher.open(&bytes).is_err());
    }
    #[test]
    fn reorders_and_waits_only_for_proven_gaps() {
        let now = Instant::now();
        let mut queue = PlayoutBuffer::new(10).unwrap();
        assert!(queue.push(0, vec![(1, 1); 120], now));
        assert!(queue.push(2, vec![(3, 3); 120], now));
        assert!(queue.push(1, vec![(2, 2); 120], now));
        assert!(!queue.push(1, vec![(4, 4); 120], now));
        assert_eq!(
            queue.pop_due(now + Duration::from_millis(10)).unwrap()[0],
            (1, 1)
        );
        assert!(!queue.push(0, vec![(4, 4); 120], now));
        assert_eq!(
            queue.pop_due(now + Duration::from_micros(12_500)).unwrap()[0],
            (2, 2)
        );
        assert_eq!(
            queue.pop_due(now + Duration::from_millis(15)).unwrap()[0],
            (3, 3)
        );
        assert!(queue.pop_due(now + Duration::from_secs(3)).is_none());
        assert_eq!(queue.lost(), 0);
        assert!(queue.push(4, vec![(5, 5); 120], now + Duration::from_secs(3)));
        assert_eq!(
            queue.pop_due(now + Duration::from_millis(3010)).unwrap()[48],
            (0, 0)
        );
        assert_eq!(queue.lost(), 1);
    }
    #[test]
    fn recovers_after_sender_pause_without_inventing_future_losses() {
        let now = Instant::now();
        let mut queue = PlayoutBuffer::new(10).unwrap();
        queue.push(0, vec![(1, 1); 120], now);
        queue.pop_due(now + Duration::from_millis(10)).unwrap();
        for _ in 0..1200 {
            assert!(queue.pop_due(now + Duration::from_secs(3)).is_none());
        }
        assert_eq!(queue.lost(), 0);
        assert!(queue.push(1, vec![(2, 2); 120], now + Duration::from_secs(3)));
        assert_eq!(
            queue.pop_due(now + Duration::from_millis(3009)).unwrap()[0],
            (2, 2)
        );
        assert!(queue.push(100, vec![(3, 3); 120], now + Duration::from_secs(4)));
        assert_eq!(queue.lost(), 98);
        assert_eq!(
            queue.pop_due(now + Duration::from_millis(4010)).unwrap()[48],
            (3, 3)
        );
        assert!(!queue.push(1, vec![(2, 2); 120], now + Duration::from_secs(4)));
    }
    #[test]
    fn upstream_buffer_can_be_fixed_at_ten_ms() {
        let mut buffer = rushaudio::JitterBuffer::new();
        buffer.set_min_delay(10);
        buffer.adapt_delay();
        assert_eq!(buffer.stats().target_delay_ms, 20.0);
        buffer.set_max_delay(10);
        buffer.adapt_delay();
        assert_eq!(buffer.stats().target_delay_ms, 10.0);
    }
    #[test]
    fn sender_catches_up_and_scheduler_stalls_do_not_invent_network_losses() {
        let now = Instant::now();
        let mut pacer = PacketPacer::new(now);
        assert_eq!(pacer.take_due(now).unwrap(), Some(0));
        let mut due = Vec::new();
        while let Some(sequence) = pacer.take_due(now + Duration::from_millis(10)).unwrap() {
            due.push(sequence);
        }
        assert_eq!(due, vec![1, 2, 3, 4]);
        assert_eq!(
            pacer.take_due(now + Duration::from_millis(100)).unwrap(),
            Some(5)
        );
        assert_eq!(
            pacer.take_due(now + Duration::from_millis(100)).unwrap(),
            None
        );
        assert_eq!(
            pacer
                .take_due(now + Duration::from_micros(102_500))
                .unwrap(),
            Some(6)
        );
    }
    #[test]
    fn buffer_changes_adjust_the_live_gap_deadline_and_retain_queued_audio() {
        let now = Instant::now();
        let mut queue = PlayoutBuffer::new(10).unwrap();
        queue.push(0, vec![(1, 1); 120], now);
        queue.pop_due(now).unwrap();
        queue.push(2, vec![(2, 2); 120], now);
        queue.set_delay(30).unwrap();
        assert!(queue.pop_due(now + Duration::from_millis(20)).is_none());
        queue.set_delay(5).unwrap();
        assert!(queue.pop_due(now + Duration::from_millis(5)).is_some());
        assert_eq!(
            queue.pop_due(now + Duration::from_millis(5)).unwrap()[60],
            (2, 2)
        );
        assert_eq!(queue.lost(), 1);
        assert!(queue.set_delay(0).is_err());
    }
    #[test]
    fn missing_packet_fades_to_silence_and_recovery_fades_in() {
        let now = Instant::now();
        let mut queue = PlayoutBuffer::new(10).unwrap();
        queue.push(0, vec![(30000, 30000); 120], now);
        assert_eq!(queue.pop_due(now).unwrap()[119], (30000, 30000));
        queue.push(
            2,
            vec![(-30000, -30000); 120],
            now + Duration::from_micros(2500),
        );
        assert!(queue.pop_due(now + Duration::from_millis(9)).is_none());
        let concealed = queue.pop_due(now + Duration::from_millis(10)).unwrap();
        assert_eq!(concealed[0], (30000, 30000));
        assert_eq!(concealed[48], (0, 0));
        let resumed = queue.pop_due(now + Duration::from_millis(10)).unwrap();
        assert_eq!(resumed[0], (0, 0));
        assert_eq!(resumed[48], (-30000, -30000));
        assert!(
            resumed
                .windows(2)
                .all(|pair| (pair[1].0 as i32 - pair[0].0 as i32).abs() <= 625)
        );
    }
}
