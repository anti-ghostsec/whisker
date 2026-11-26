//! The sound of a call: one connection to the other side, through the
//! account's server's relay only (see [`crate::calls`]), carrying the
//! microphone one way and the other person's voice the other, as Opus.
//!
//! The pieces: [`Link`] is the connection (what's offered, answered, and
//! sent down it), [`Playout`] is where the other side's voice waits to be
//! played, a moment's worth at a time so sound that arrives unevenly still
//! plays evenly.

use crate::calls::{self, Candidate};
use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};

use rtc::ice::mdns::MulticastDnsMode;
use rtc::media::Sample;
use rtc::peer_connection::configuration::media_engine::MIME_TYPE_OPUS;
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind};
use webrtc::media_stream::MediaStreamTrack;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceGatheringState, RTCIceServer, RTCIceTransportPolicy,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState, RTCSessionDescription, Registry, SettingEngineBuilder, register_default_interceptors,
};
use webrtc::rtp_transceiver::RtpSender;

/// Sound a second, and how much of it goes in one piece (20 ms), one channel.
pub const RATE: u32 = 48_000;
pub const FRAME: usize = 960;

/// How long the relay is given to hand out an address before the call is
/// offered with what there is.
const GATHER_FOR: Duration = Duration::from_secs(8);

// ---- Opus ----

/// Turns microphone sound into Opus, tuned for speech.
pub struct Encoder(*mut unsafe_libopus::OpusEncoder);
// SAFETY: the encoder's state is its own memory, touched only through
// `&mut self`.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new() -> Result<Self> {
        use unsafe_libopus::{OPUS_APPLICATION_VOIP, OPUS_OK, OPUS_SET_BITRATE_REQUEST, OPUS_SET_INBAND_FEC_REQUEST, OPUS_SET_PACKET_LOSS_PERC_REQUEST, opus_encoder_create, opus_encoder_ctl};
        let mut err = 0i32;
        // SAFETY: valid arguments; the result is checked before use.
        let st = unsafe { opus_encoder_create(RATE as i32, 1, OPUS_APPLICATION_VOIP, &mut err) };
        if err != OPUS_OK || st.is_null() {
            bail!("the sound encoder wouldn't start ({err})");
        }
        // SAFETY: `st` is a live encoder; these requests take one number.
        unsafe {
            opus_encoder_ctl!(st, OPUS_SET_BITRATE_REQUEST, 32_000);
            // Each piece carries a rough copy of the one before, to fill in
            // for one that's lost on the way.
            opus_encoder_ctl!(st, OPUS_SET_INBAND_FEC_REQUEST, 1);
            opus_encoder_ctl!(st, OPUS_SET_PACKET_LOSS_PERC_REQUEST, 10);
        }
        Ok(Self(st))
    }

    /// One piece of sound ([`FRAME`] samples) as an Opus packet.
    pub fn encode(&mut self, pcm: &[i16; FRAME]) -> Result<Vec<u8>> {
        let mut out = vec![0u8; 1276];
        // SAFETY: `pcm` holds FRAME samples of one channel and `out` is as long as said.
        let n = unsafe { unsafe_libopus::opus_encode(self.0, pcm.as_ptr(), FRAME as i32, out.as_mut_ptr(), out.len() as i32) };
        if n < 0 {
            bail!("the sound encoder failed ({n})");
        }
        out.truncate(n as usize);
        Ok(out)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: made by opus_encoder_create, destroyed once.
        unsafe { unsafe_libopus::opus_encoder_destroy(self.0) }
    }
}

/// Turns the other side's Opus back into sound.
pub struct Decoder(*mut unsafe_libopus::OpusDecoder);
// SAFETY: as for the encoder.
unsafe impl Send for Decoder {}

/// The longest one packet can be (120 ms).
const LONGEST: usize = 5760;

impl Decoder {
    pub fn new() -> Result<Self> {
        let mut err = 0i32;
        // SAFETY: valid arguments; the result is checked before use.
        let st = unsafe { unsafe_libopus::opus_decoder_create(RATE as i32, 1, &mut err) };
        if err != unsafe_libopus::OPUS_OK || st.is_null() {
            bail!("the sound decoder wouldn't start ({err})");
        }
        Ok(Self(st))
    }

    fn run(&mut self, packet: Option<&[u8]>, samples: usize, fec: bool) -> Vec<i16> {
        let mut pcm = vec![0i16; samples];
        let (data, len) = packet.map_or((std::ptr::null(), 0), |p| (p.as_ptr(), p.len() as i32));
        // SAFETY: `data` is `len` bytes (or null with 0, which asks for a
        // fill-in), and `pcm` holds `samples` samples of one channel.
        let n = unsafe { unsafe_libopus::opus_decode(self.0, data, len, pcm.as_mut_ptr(), samples as i32, i32::from(fec)) };
        pcm.truncate(n.max(0) as usize);
        pcm
    }

    /// The sound in `packet` (nothing, when it isn't Opus).
    pub fn decode(&mut self, packet: &[u8]) -> Vec<i16> {
        if packet.is_empty() || packet.len() > 1500 { Vec::new() } else { self.run(Some(packet), LONGEST, false) }
    }

    /// A guess at a piece that never came, from the sound before it.
    pub fn fill_in(&mut self) -> Vec<i16> {
        self.run(None, FRAME, false)
    }

    /// The piece before `next` that never came, from the rough copy `next`
    /// carries of it.
    pub fn recover(&mut self, next: &[u8]) -> Vec<i16> {
        if next.is_empty() || next.len() > 1500 { self.fill_in() } else { self.run(Some(next), FRAME, true) }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: made by opus_decoder_create, destroyed once.
        unsafe { unsafe_libopus::opus_decoder_destroy(self.0) }
    }
}

// ---- what's heard ----

/// The other side's voice, waiting to be played. Sound arrives in bursts
/// and gaps; a little is kept in hand (60 ms) so it plays evenly, and
/// never much (a third of a second), so the two sides don't drift into
/// talking over each other.
#[derive(Default)]
pub struct Playout {
    held: Mutex<Held>,
}

#[derive(Default)]
struct Held {
    pcm: VecDeque<i16>,
    /// Playing has started (enough was in hand).
    flowing: bool,
}

const IN_HAND: usize = FRAME * 3;
const AT_MOST: usize = RATE as usize / 3;

impl Playout {
    fn push(&self, pcm: &[i16]) {
        let mut h = self.held.lock().unwrap();
        h.pcm.extend(pcm);
        // Too much waiting: the oldest goes, back to what's kept in hand.
        if h.pcm.len() > AT_MOST {
            let drop = h.pcm.len() - IN_HAND;
            h.pcm.drain(..drop);
        }
    }

    /// The next piece to play: silence while there's too little in hand.
    pub fn take(&self, out: &mut [i16]) {
        let mut h = self.held.lock().unwrap();
        if !h.flowing && h.pcm.len() < IN_HAND.max(out.len()) {
            out.fill(0);
            return;
        }
        h.flowing = true;
        let n = out.len().min(h.pcm.len());
        for (o, s) in out.iter_mut().zip(h.pcm.drain(..n)) {
            *o = s;
        }
        // Ran dry: silence for the rest, and wait to have some in hand again.
        if n < out.len() {
            out[n..].fill(0);
            h.flowing = false;
        }
    }
}

/// Decodes what arrives on the other side's track into `playout`, in
/// order: pieces that never came are filled in (up to a few in a row),
/// ones that come late are left out.
async fn hear(track: Arc<dyn TrackRemote>, playout: Arc<Playout>) {
    let Ok(mut decoder) = Decoder::new() else { return };
    let mut last: Option<u16> = None;
    while let Some(event) = track.poll().await {
        let packet = match event {
            TrackRemoteEvent::OnRtpPacket(p) => p,
            TrackRemoteEvent::OnEnded => break,
            _ => continue,
        };
        let seq = packet.header.sequence_number;
        if let Some(l) = last {
            let ahead = seq.wrapping_sub(l);
            // Behind what's been played, or the same again: too late.
            if ahead == 0 || ahead > u16::MAX / 2 {
                continue;
            }
            // Up to three missing: the first ones guessed, the last one
            // rebuilt from the copy this packet carries.
            if (2..=4).contains(&ahead) {
                for _ in 0..ahead - 2 {
                    playout.push(&decoder.fill_in());
                }
                playout.push(&decoder.recover(&packet.payload));
            }
        }
        last = Some(seq);
        playout.push(&decoder.decode(&packet.payload));
    }
}

// ---- the connection ----

/// How the call's sound travels.
pub enum Route {
    /// Through the account's server's relay: the only way a call is made.
    Relay(calls::Relay),
    /// Straight between two ends on this computer (tests have no relay).
    #[cfg(test)]
    Loopback,
}

/// What a [`Link`] tells whoever made it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkEvent {
    /// Another way to reach this side (a relay's address) to tell the other.
    Candidate(Candidate),
    /// Sound can flow.
    Connected,
    /// It was lost (it may come back by itself).
    Interrupted,
    /// It's over: lost for good, or closed.
    Ended,
}

struct Handler {
    events: mpsc::UnboundedSender<LinkEvent>,
    gathered: Arc<Notify>,
    playout: Arc<Playout>,
    direct: bool,
}

// These run on the connection's own task: they hand off and return.
#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        let Ok(init) = event.candidate.to_json() else { return };
        // Only a relay's address is ever told to the other side.
        if !self.direct && !calls::is_relay(&init.candidate) {
            return;
        }
        let candidate = if self.direct { init.candidate } else { calls::hide_related(&init.candidate) };
        // The one sound line of the call, whatever the library calls it.
        let _ = self.events.send(LinkEvent::Candidate(Candidate { candidate, sdp_mid: Some("0".into()), sdp_mline_index: Some(0) }));
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathered.notify_one();
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let event = match state {
            RTCPeerConnectionState::Connected => LinkEvent::Connected,
            RTCPeerConnectionState::Disconnected => LinkEvent::Interrupted,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => LinkEvent::Ended,
            _ => return,
        };
        let _ = self.events.send(event);
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        tokio::spawn(hear(track, self.playout.clone()));
    }
}

/// One call's connection.
pub struct Link {
    pc: Arc<dyn PeerConnection>,
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    ssrc: u32,
    /// The number the two sides agreed for Opus, once known.
    payload_type: Mutex<Option<u8>>,
    encoder: tokio::sync::Mutex<Encoder>,
    gathered: Arc<Notify>,
    playout: Arc<Playout>,
    direct: bool,
}

/// The relay addresses Whisker can use: over UDP (the library reaches a
/// relay no other way yet), with nothing odd in them.
fn usable(relay: &calls::Relay) -> Vec<String> {
    relay
        .uris
        .iter()
        .filter(|u| u.starts_with("turn:") && !u.contains("transport=tcp") && u.len() < 300 && u.chars().all(|c| c.is_ascii_graphic()))
        .cloned()
        .collect()
}

impl Link {
    /// A connection ready to offer or answer, and what it reports.
    pub async fn new(route: Route) -> Result<(Self, mpsc::UnboundedReceiver<LinkEvent>)> {
        let opus = RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: RATE,
                // Opus is always described as two channels, even for one.
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
        };
        let mut media = MediaEngine::default();
        media.register_codec(opus.clone(), RtpCodecKind::Audio).map_err(|e| anyhow!("{e}"))?;
        let registry = register_default_interceptors(Registry::new(), &mut media).map_err(|e| anyhow!("{e}"))?;

        let (config, listen, direct) = match route {
            Route::Relay(relay) => {
                let urls = usable(&relay);
                if urls.is_empty() {
                    bail!("this account's server has a relay for calls, but only one reached over TCP, which Whisker can't use yet");
                }
                let server = RTCIceServer { urls, username: relay.username, credential: relay.password };
                // Relay only: no address of this device is ever looked up or offered.
                (RTCConfigurationBuilder::new().with_ice_servers(vec![server]).with_ice_transport_policy(RTCIceTransportPolicy::Relay).build(), "0.0.0.0:0", false)
            }
            #[cfg(test)]
            Route::Loopback => (RTCConfigurationBuilder::new().build(), "127.0.0.1:0", true),
        };
        // No looking up or announcing names on the local network.
        let settings = SettingEngineBuilder::new().with_multicast_dns_mode(MulticastDnsMode::Disabled).build();

        let (events, reports) = mpsc::unbounded_channel();
        let gathered = Arc::new(Notify::new());
        let playout = Arc::new(Playout::default());
        let handler = Handler { events, gathered: gathered.clone(), playout: playout.clone(), direct };
        let pc = PeerConnectionBuilder::new()
            .with_configuration(config)
            .with_media_engine(media)
            .with_setting_engine(settings)
            .with_interceptor_registry(registry)
            .with_handler(Arc::new(handler))
            .with_udp_addrs(vec![listen.to_owned()])
            .build()
            .await
            .map_err(|e| anyhow!("{e}"))
            .context("couldn't set up the call's connection")?;
        let pc: Arc<dyn PeerConnection> = Arc::new(pc);

        let mut number = [0u8; 4];
        getrandom::fill(&mut number).map_err(|e| anyhow!("{e}"))?;
        let ssrc = u32::from_le_bytes(number) | 1;
        let stream = MediaStreamTrack::new(
            "whisker".to_owned(),
            "whisker-audio".to_owned(),
            "audio".to_owned(),
            RtpCodecKind::Audio,
            vec![RTCRtpEncodingParameters { rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() }, codec: opus.rtp_codec.clone(), ..Default::default() }],
        );
        let track = Arc::new(TrackLocalStaticSample::new(Instant::now(), stream).map_err(|e| anyhow!("{e}"))?);
        let sender = pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>).await.map_err(|e| anyhow!("{e}"))?;
        let link = Self { pc, track, sender, ssrc, payload_type: Mutex::new(None), encoder: tokio::sync::Mutex::new(Encoder::new()?), gathered, playout, direct };
        Ok((link, reports))
    }

    /// Where the other side's voice waits to be played.
    pub fn playout(&self) -> Arc<Playout> {
        self.playout.clone()
    }

    /// This side's session description once the relay has given its
    /// address (or the wait for it is up), with only the relay in it.
    async fn described(&self) -> Result<String> {
        let _ = tokio::time::timeout(GATHER_FOR, self.gathered.notified()).await;
        let sdp = self.pc.local_description().await.map(|d| d.sdp).ok_or_else(|| anyhow!("the call has nothing to offer"))?;
        if self.direct {
            return Ok(sdp);
        }
        let sdp = calls::relay_only(&sdp);
        if !sdp.contains("a=candidate:") {
            bail!("the server's relay didn't answer, so the call can't be made privately");
        }
        Ok(sdp)
    }

    /// Starts a call: what to send the other side.
    pub async fn offer(&self) -> Result<String> {
        let offer = self.pc.create_offer(None).await.map_err(|e| anyhow!("{e}"))?;
        self.pc.set_local_description(offer).await.map_err(|e| anyhow!("{e}"))?;
        self.described().await
    }

    /// Picks up a call offered as `offer`: what to send back.
    pub async fn accept(&self, offer: &str) -> Result<String> {
        let offer = RTCSessionDescription::offer(offer.to_owned()).map_err(|e| anyhow!("{e}"))?;
        self.pc.set_remote_description(offer).await.map_err(|e| anyhow!("{e}"))?;
        let answer = self.pc.create_answer(None).await.map_err(|e| anyhow!("{e}"))?;
        self.pc.set_local_description(answer).await.map_err(|e| anyhow!("{e}"))?;
        self.described().await
    }

    /// The other side picked up, with `answer`.
    pub async fn answered(&self, answer: &str) -> Result<()> {
        let answer = RTCSessionDescription::answer(answer.to_owned()).map_err(|e| anyhow!("{e}"))?;
        self.pc.set_remote_description(answer).await.map_err(|e| anyhow!("{e}"))
    }

    /// Another way to reach the other side. (Only after its offer or
    /// answer is in: keep earlier ones until then.)
    pub async fn reach(&self, c: &Candidate) -> Result<()> {
        let init = RTCIceCandidateInit { candidate: c.candidate.clone(), sdp_mid: c.sdp_mid.clone(), sdp_mline_index: c.sdp_mline_index, ..Default::default() };
        self.pc.add_ice_candidate(init).await.map_err(|e| anyhow!("{e}"))
    }

    /// Sends one piece of microphone sound.
    pub async fn say(&self, pcm: &[i16; FRAME]) -> Result<()> {
        let packet = self.encoder.lock().await.encode(pcm)?;
        let known = *self.payload_type.lock().unwrap();
        let payload_type = match known {
            Some(pt) => pt,
            None => {
                let pt = self.sender.get_parameters().await.map_err(|e| anyhow!("{e}"))?.rtp_parameters.codecs.first().map_or(111, |c| c.payload_type);
                *self.payload_type.lock().unwrap() = Some(pt);
                pt
            }
        };
        let sample = Sample { data: Bytes::from(packet), duration: Duration::from_millis(20), ..Sample::new(Instant::now()) };
        self.track.sample_writer(self.ssrc, payload_type).write_sample(&sample).await.map_err(|e| anyhow!("{e}"))
    }

    /// Ends it (and gives the relay's address back).
    pub async fn close(&self) {
        let _ = self.pc.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, from: usize) -> [i16; FRAME] {
        std::array::from_fn(|n| (((from + n) as f32 * hz * 2.0 * std::f32::consts::PI / RATE as f32).sin() * 10_000.0) as i16)
    }

    fn pitch_of(samples: &[i16]) -> f32 {
        let ups = samples.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
        ups as f32 * RATE as f32 / samples.len() as f32
    }

    #[test]
    fn opus_carries_a_tone_and_fills_in_for_a_lost_piece() {
        let (mut enc, mut dec) = (Encoder::new().unwrap(), Decoder::new().unwrap());
        let mut heard = Vec::new();
        for i in 0..50 {
            let packet = enc.encode(&tone(440.0, i * FRAME)).unwrap();
            assert!(packet.len() < 400, "{} bytes for 20 ms", packet.len());
            // One piece lost on the way.
            if i == 30 {
                heard.extend(dec.fill_in());
            } else {
                heard.extend(dec.decode(&packet));
            }
        }
        assert_eq!(heard.len(), 50 * FRAME);
        let got = pitch_of(&heard[FRAME * 5..]);
        assert!((got - 440.0).abs() < 15.0, "{got} Hz");
        // Junk isn't sound.
        assert!(dec.decode(&[]).is_empty());
    }

    #[test]
    fn what_is_heard_plays_evenly() {
        let p = Playout::default();
        let mut out = [1i16; FRAME];
        // Too little in hand: silence, and it's kept.
        p.push(&[5; FRAME]);
        p.take(&mut out);
        assert!(out.iter().all(|&s| s == 0));
        p.push(&[5; FRAME * 2]);
        p.take(&mut out);
        assert!(out.iter().all(|&s| s == 5));
        // A flood: only the newest third of a second or less stays.
        p.push(&vec![7; RATE as usize]);
        assert!(p.held.lock().unwrap().pcm.len() <= AT_MOST);
    }

    #[test]
    fn only_udp_relays_are_used() {
        let relay = calls::Relay { uris: vec!["turn:r.example:3478?transport=udp".into(), "turn:r.example:3478?transport=tcp".into(), "turns:r.example:5349".into(), "turn:r.example:3478".into(), "turn:bad host".into()], username: "u".into(), password: "p".into() };
        assert_eq!(usable(&relay), ["turn:r.example:3478?transport=udp", "turn:r.example:3478"]);
    }

    /// Two ends on this computer: one calls, the other picks up, and a
    /// tone said at one end is heard at the other.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tone_said_at_one_end_is_heard_at_the_other() {
        let (caller, mut caller_says) = Link::new(Route::Loopback).await.unwrap();
        let (called, mut called_says) = Link::new(Route::Loopback).await.unwrap();
        let offer = caller.offer().await.unwrap();
        let answer = called.accept(&offer).await.unwrap();
        caller.answered(&answer).await.unwrap();

        let connected = |mut says: mpsc::UnboundedReceiver<LinkEvent>| async move {
            while let Some(e) = says.recv().await {
                if e == LinkEvent::Connected {
                    return says;
                }
            }
            panic!("never connected");
        };
        caller_says = tokio::time::timeout(Duration::from_secs(20), connected(caller_says)).await.expect("the caller connects");
        called_says = tokio::time::timeout(Duration::from_secs(20), connected(called_says)).await.expect("the called connects");
        let _ = (&mut caller_says, &mut called_says);

        // Two seconds of a tone, a piece every 20 ms, played out as it comes.
        let playout = called.playout();
        let mut heard = Vec::new();
        let mut every = tokio::time::interval(Duration::from_millis(20));
        for i in 0..100 {
            every.tick().await;
            caller.say(&tone(440.0, i * FRAME)).await.unwrap();
            let mut piece = [0i16; FRAME];
            playout.take(&mut piece);
            heard.extend(piece);
        }
        let sound: Vec<i16> = heard.iter().copied().skip_while(|&s| s == 0).collect();
        assert!(sound.len() > RATE as usize, "only {} samples heard", sound.len());
        let got = pitch_of(&sound[FRAME * 5..]);
        assert!((got - 440.0).abs() < 25.0, "{got} Hz");
        caller.close().await;
        called.close().await;
    }
}
