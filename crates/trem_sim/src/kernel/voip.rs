//! Kernel `voip` — ioquake3's in-game voice chat as tremulous-legacy builds
//! it (USE_VOIP, Opus): the client half of client/cl_main.c (`CL_Voip_f`,
//! `CL_UpdateVoipIgnore`, `CL_UpdateVoipGain`, `CL_VoipNewGeneration`,
//! `CL_VoipParseTargets`, `CL_CaptureVoip`), client/cl_input.c
//! (`CL_WritePacket`'s clc_voipOpus block, `IN_VoipRecordDown` /
//! `IN_VoipRecordUp`), client/cl_parse.c (`CL_ShouldIgnoreVoipSender`,
//! `CL_PlayVoip`, `CL_ParseVoip`), client/cl_cgame.c (`CL_FirstSnapshot`'s
//! codec start), client/cl_scrn.c (`SCR_DrawVoipMeter`'s text), the server
//! half of server/sv_client.c (`SV_UpdateVoipIgnore`, `SV_Voip_f`,
//! `SV_ShouldIgnoreVoipSender`, `SV_UserVoip`, `SV_FreeClient`'s queue),
//! server/sv_snapshot.c (`SV_WriteVoipToClient`) and qcommon/common.c
//! (`Com_IsVoipTarget`). The codec is the bundled libopus 1.1 (trem_opus).
//!
//! Sound I/O and cvars belong to the host (`VoipHost`: `S_StartCapture`,
//! `S_AvailableCaptureSamples`, `S_Capture`, `S_StopCapture`,
//! `S_MasterGain`, `Cvar_*`, `CG_LAST_ATTACKER` / `CG_CROSSHAIR_PLAYER`);
//! decoded voice leaves as `VoipPlayback` (`S_RawSamples` streams).

use super::cl::{ConnState, NetClient};
use super::msg::Msg;
use super::sv::{clc, svc, ClientState, Server};
use super::svgame;
use std::collections::VecDeque;
use trem_opus::{Decoder, Encoder, OPUS_APPLICATION_VOIP};

/// `MAX_CLIENTS`.
pub const MAX_CLIENTS: usize = 64;
/// `VOIP_SPATIAL`: spatialized voip message.
pub const VOIP_SPATIAL: i32 = 0x01;
/// `VOIP_DIRECT`: non-spatialized voip message.
pub const VOIP_DIRECT: i32 = 0x02;
/// `VOIP_FLAGCNT`: number of flags voip knows.
pub const VOIP_FLAGCNT: i32 = 2;
/// `VOIP_MAX_FRAME_SAMPLES`: 20ms at 48k.
pub const VOIP_MAX_FRAME_SAMPLES: usize = 20 * 48;
/// `VOIP_MAX_PACKET_FRAMES`: 3 frame is 60ms of audio, the max opus will encode at once.
pub const VOIP_MAX_PACKET_FRAMES: usize = 3;
/// `VOIP_MAX_PACKET_SAMPLES`.
pub const VOIP_MAX_PACKET_SAMPLES: usize = VOIP_MAX_FRAME_SAMPLES * VOIP_MAX_PACKET_FRAMES;
/// `VOIP_QUEUE_LENGTH`.
pub const VOIP_QUEUE_LENGTH: usize = 64;
/// `sizeof( clc.voipTargets )`.
pub const VOIP_TARGET_BYTES: usize = MAX_CLIENTS.div_ceil(8);
/// `sizeof( voipServerPacket_t.data )`.
const VOIP_SERVER_DATA: usize = 4000;
/// `sizeof( clc.voipOutgoingData )`.
const VOIP_OUTGOING_DATA: usize = 1024;
/// `MAX_RAW_STREAMS`'s first VoIP stream: `sender + 1` direct, `sender + MAX_CLIENTS + 1` spatial.
pub const VOIP_STREAM_BASE: usize = 1;

/// `Com_IsVoipTarget`: non-zero if given clientNum is enabled in
/// voipTargets; a negative clientNum asks whether any bit is set.
pub fn is_voip_target(targets: &[u8], client_num: i32) -> bool {
    if client_num < 0 {
        return targets.iter().any(|&t| t != 0);
    }
    let index = (client_num >> 3) as usize;
    index < targets.len() && targets[index] & (1 << (client_num & 0x07)) != 0
}

// =====================================================================
// server (sv_client.c / sv_snapshot.c / sv_init.c)
// =====================================================================

/// `voipServerPacket_t`.
#[derive(Clone, Debug, Default)]
pub struct VoipServerPacket {
    pub generation: i32,
    pub sequence: i32,
    pub frames: i32,
    pub len: i32,
    pub sender: i32,
    pub flags: i32,
    pub data: Vec<u8>,
}

/// The `client_t` VoIP fields.
#[derive(Clone, Debug)]
pub struct SvClientVoip {
    pub has_voip: bool,
    pub mute_all_voip: bool,
    pub ignore_voip_from_client: [bool; MAX_CLIENTS],
    /// `voipPacket[ queuedVoipIndex .. +queuedVoipPackets ]`
    pub queue: VecDeque<VoipServerPacket>,
}

impl Default for SvClientVoip {
    fn default() -> Self {
        SvClientVoip { has_voip: false, mute_all_voip: false, ignore_voip_from_client: [false; MAX_CLIENTS], queue: VecDeque::new() }
    }
}

impl SvClientVoip {
    /// `SV_UserinfoChanged`'s `cl->hasVoip`.
    pub fn userinfo_changed(&mut self, userinfo: &str) {
        self.has_voip = svgame::info_value(userinfo, "cl_voipProtocol").eq_ignore_ascii_case("opus");
    }

    /// `SV_FreeClient`: the queued packets go.
    pub fn free(&mut self) {
        self.queue.clear();
    }

    /// `SV_UpdateVoipIgnore`.
    fn update_ignore(&mut self, idstr: &str, ignore: bool) {
        if idstr.as_bytes().first().is_some_and(|c| c.is_ascii_digit()) {
            let id = super::spawn::c_atoi(idstr);
            if (0..MAX_CLIENTS as i32).contains(&id) {
                self.ignore_voip_from_client[id as usize] = ignore;
            }
        }
    }

    /// `SV_Voip_f`: the "voip" client command.
    pub fn voip_f(&mut self, argv: &[String]) {
        let a = |i: usize| argv.get(i).map(|s| s.as_str()).unwrap_or("");
        match a(1) {
            "ignore" => self.update_ignore(a(2), true),
            "unignore" => self.update_ignore(a(2), false),
            "muteall" => self.mute_all_voip = true,
            "unmuteall" => self.mute_all_voip = false,
            _ => {}
        }
    }

    /// `SV_WriteVoipToClient`: as many queued packets as reasonably fit
    /// (none while the client downloads; they are dropped then).
    pub fn write_to_client(&mut self, downloading: bool, m: &mut Msg) {
        let mut totalbytes = 0;
        while let Some(packet) = self.queue.front() {
            if !downloading {
                totalbytes += packet.len;
                if totalbytes > (m.maxsize() as i32 - m.cursize as i32) / 2 {
                    break;
                }
                m.write_byte(svc::VOIP_OPUS);
                m.write_short(packet.sender);
                m.write_byte(packet.generation as u8 as i32);
                m.write_long(packet.sequence);
                m.write_byte(packet.frames);
                m.write_short(packet.len);
                m.write_bits(packet.flags, VOIP_FLAGCNT);
                m.write_data(&packet.data[..packet.len as usize]);
            }
            self.queue.pop_front();
        }
    }
}

/// `sv_voip` / `sv_voipProtocol` (sv_init.c: "opus" when sv_voip is on).
pub fn voip_protocol(sv_voip: i32) -> &'static str {
    if sv_voip != 0 {
        "opus"
    } else {
        ""
    }
}

impl Server {
    /// `SV_ShouldIgnoreVoipSender`: blocking of voip packets based on source client.
    fn should_ignore_voip_sender(&self, i: usize) -> bool {
        // VoIP disabled on this server, or the client doesn't have VoIP support
        self.cvars.voip == 0 || !self.clients[i].voip.has_voip
    }

    /// `SV_UserVoip`: a clc_voipOpus (or the skipped clc_voipSpeex) block
    /// of client `i`, queued for every client it is addressed to.
    pub(super) fn user_voip(&mut self, i: usize, m: &mut Msg, ignore_data: bool) {
        let sender = i as i32;
        let generation = m.read_byte();
        let sequence = m.read_long();
        let frames = m.read_byte();
        let recips = m.read_data(VOIP_TARGET_BYTES);
        let mut flags = m.read_byte();
        let packetsize = m.read_short();
        if m.readcount > m.cursize || packetsize < 0 {
            return; // short/invalid packet, bail.
        }
        if packetsize as usize > VOIP_SERVER_DATA {
            // overlarge packet?
            let mut bytesleft = packetsize as usize;
            while bytesleft > 0 {
                let br = bytesleft.min(VOIP_SERVER_DATA);
                m.read_data(br);
                bytesleft -= br;
            }
            return; // overlarge packet, bail.
        }
        let encoded = m.read_data(packetsize.max(0) as usize);
        if ignore_data || self.should_ignore_voip_sender(i) {
            return; // Blacklisted, disabled, etc.
        }
        let downloading = !self.clients[i].download.name.is_empty();
        // decide who needs this VoIP packet sent to them...
        let maxclients = (self.cvars.maxclients.max(0) as usize).min(self.clients.len());
        for j in 0..maxclients {
            let client = &self.clients[j];
            if client.state != ClientState::Active {
                continue; // not in the game yet, don't send to this guy.
            } else if j == i {
                continue; // don't send voice packet back to original author.
            } else if !client.voip.has_voip {
                continue; // no VoIP support, or unsupported protocol
            } else if client.voip.mute_all_voip {
                continue; // client is ignoring everyone.
            } else if client.voip.ignore_voip_from_client[i] {
                continue; // client is ignoring this talker.
            } else if downloading {
                continue; // no VoIP allowed if downloading, to save bandwidth.
            }
            if is_voip_target(&recips, j as i32) {
                flags |= VOIP_DIRECT;
            } else {
                flags &= !VOIP_DIRECT;
            }
            if flags & (VOIP_SPATIAL | VOIP_DIRECT) == 0 {
                continue; // not addressed to this player.
            }
            // Transmit this packet to the client.
            if client.voip.queue.len() >= VOIP_QUEUE_LENGTH {
                self.console.push_str(&format!("Too many VoIP packets queued for client #{j}\n"));
                continue; // no room for another packet right now.
            }
            let packet = VoipServerPacket { sender, frames, len: packetsize, generation, sequence, flags, data: encoded.clone() };
            self.clients[j].voip.queue.push_back(packet);
        }
    }

    /// `SV_ExecuteClientMessage`'s optional voip blocks: the legacy speex
    /// data is skipped, the opus data relayed; the next command byte.
    pub(super) fn read_client_voip(&mut self, i: usize, m: &mut Msg, mut c: i32) -> i32 {
        // skip legacy speex voip data
        if c == clc::VOIP_SPEEX {
            self.user_voip(i, m, true);
            c = m.read_byte();
        }
        // read optional voip data
        if c == clc::VOIP_OPUS {
            self.user_voip(i, m, false);
            c = m.read_byte();
        }
        c
    }
}

// =====================================================================
// client (cl_main.c / cl_input.c / cl_parse.c / cl_cgame.c / cl_scrn.c)
// =====================================================================

/// One `S_RawSamples` call of `CL_PlayVoip`: 48 kHz mono 16-bit.
#[derive(Clone, Debug, PartialEq)]
pub struct VoipPlayback {
    /// raw stream: `sender + 1` (direct) or `sender + MAX_CLIENTS + 1` (spatial)
    pub stream: usize,
    pub sender: usize,
    pub samples: Vec<i16>,
    /// clc.voipGain[ sender ] (direct) or 1.0 (spatial)
    pub volume: f32,
    /// the entity the stream is spatialized at (`sender`), -1 for none
    pub entity: i32,
}

/// The `clc` VoIP fields (`clientConnection_t`).
pub struct ClVoip {
    pub enabled: bool,
    pub codec_initialized: bool,
    pub opus_decoder: Vec<Decoder>,
    pub incoming_generation: [u8; MAX_CLIENTS],
    pub incoming_sequence: [i32; MAX_CLIENTS],
    pub gain: [f32; MAX_CLIENTS],
    pub ignore: [bool; MAX_CLIENTS],
    pub mute_all: bool,
    /// if voipTargets[i / 8] & (1 << (i % 8)), then we are sending to clientnum i.
    pub targets: [u8; VOIP_TARGET_BYTES],
    pub flags: u8,
    pub opus_encoder: Option<Encoder>,
    pub outgoing_data_size: usize,
    pub outgoing_data_frames: i32,
    pub outgoing_sequence: i32,
    pub outgoing_generation: u8,
    pub outgoing_data: [u8; VOIP_OUTGOING_DATA],
    pub power: f32,
    /// `clc.demoplaying` (set by a demo player; no demos otherwise)
    pub demoplaying: bool,
    /// decoded voice for the host's raw streams (`CL_PlayVoip`)
    pub playback: Vec<VoipPlayback>,
    /// `CL_FirstSnapshot`'s `Cvar_Set( "cl_voipSendTarget", "spatial" )`,
    /// applied by the next `capture_voip`
    pending_send_target: bool,
    /// `CL_Disconnect`'s capture clean-up, done by the next `capture_voip`
    pending_stop: bool,
    /// the capture device is open (between `S_StartCapture` and `S_StopCapture`)
    pub capturing: bool,
    /// `voip` is a registered command (`Cmd_AddCommand( "voip", CL_Voip_f )`)
    pub command_registered: bool,
    /// `cl_voip->integer` as the last `capture_voip` read it
    pub cl_voip: bool,
    /// port: why the last cl_voipSend 1 was turned back to 0 (the
    /// dontCapture reasons of CL_CaptureVoip), for the microphone toggle
    pub refused: Option<&'static str>,
}

impl Default for ClVoip {
    fn default() -> Self {
        ClVoip {
            enabled: false,
            codec_initialized: false,
            opus_decoder: Vec::new(),
            incoming_generation: [0; MAX_CLIENTS],
            incoming_sequence: [0; MAX_CLIENTS],
            gain: [0.0; MAX_CLIENTS],
            ignore: [false; MAX_CLIENTS],
            mute_all: false,
            targets: [0; VOIP_TARGET_BYTES],
            flags: 0,
            opus_encoder: None,
            outgoing_data_size: 0,
            outgoing_data_frames: 0,
            outgoing_sequence: 0,
            outgoing_generation: 0,
            outgoing_data: [0; VOIP_OUTGOING_DATA],
            power: 0.0,
            demoplaying: false,
            playback: Vec::new(),
            pending_send_target: false,
            pending_stop: false,
            capturing: false,
            command_registered: false,
            cl_voip: true,
            refused: None,
        }
    }
}

/// What `CL_CaptureVoip` needs from the engine around the client: the
/// cvars (cl_voip*, rate), the sound system's capture and master gain, and
/// the cgame's `CG_LAST_ATTACKER` / `CG_CROSSHAIR_PLAYER`.
pub trait VoipHost {
    fn cvar_string(&self, name: &str) -> String;
    fn cvar_value(&self, name: &str) -> f32 {
        super::spawn::c_atof(&self.cvar_string(name)) as f32
    }
    fn cvar_integer(&self, name: &str) -> i32 {
        super::spawn::c_atoi(&self.cvar_string(name))
    }
    /// `Cvar_Set`
    fn cvar_set(&mut self, name: &str, value: &str);
    /// the cvar's `modified` flag, cleared
    fn cvar_take_modified(&mut self, name: &str) -> bool;
    /// `S_StartCapture`
    fn start_capture(&mut self);
    /// `S_AvailableCaptureSamples` (48 kHz mono)
    fn available_capture_samples(&mut self) -> usize;
    /// `S_Capture`: `samples` 48 kHz mono 16-bit samples
    fn capture(&mut self, samples: usize) -> Vec<i16>;
    /// `S_StopCapture`
    fn stop_capture(&mut self);
    /// `S_MasterGain`
    fn master_gain(&mut self, gain: f32);
    /// `VM_Call( cgvm, CG_LAST_ATTACKER )`
    fn last_attacker(&self) -> i32 {
        -1
    }
    /// `VM_Call( cgvm, CG_CROSSHAIR_PLAYER )`
    fn crosshair_player(&self) -> i32 {
        -1
    }
}

/// `CL_Init`'s VoIP cvars (name, default, archived).
pub const CL_VOIP_CVARS: &[(&str, &str, bool)] = &[
    ("cl_voipSend", "0", false),
    ("cl_voipSendTarget", "spatial", false),
    ("cl_voipGainDuringCapture", "0.2", true),
    ("cl_voipCaptureMult", "2.0", true),
    ("cl_voipUseVAD", "0", true),
    ("cl_voipVADThreshold", "0.25", true),
    ("cl_voipShowMeter", "1", true),
    ("cl_voip", "1", true),
];

/// `cl_voipProtocol` (CVAR_USERINFO | CVAR_ROM): "opus" while cl_voip is on.
pub fn client_voip_protocol(cl_voip: i32) -> &'static str {
    voip_protocol(cl_voip)
}

/// `Q_stricmpn( target, word, n ) == 0` on the bytes left.
fn starts_ci(s: &[u8], word: &str) -> bool {
    s.len() >= word.len() && s[..word.len()].eq_ignore_ascii_case(word.as_bytes())
}

impl NetClient {
    /// `CL_FirstSnapshot`'s codec start: an encoder and a decoder per
    /// client, gains 1, nobody ignored, the "voip" command, every target.
    pub(super) fn voip_first_snapshot(&mut self) {
        if self.voip.codec_initialized {
            return;
        }
        self.voip.opus_encoder = match Encoder::new(48000, 1, OPUS_APPLICATION_VOIP) {
            Ok(e) => Some(e),
            Err(e) => {
                self.print_out.push_str(&format!("VoIP: Error opus_encoder_create {e}\n"));
                return;
            }
        };
        self.voip.opus_decoder.clear();
        for i in 0..MAX_CLIENTS {
            match Decoder::new(48000, 1) {
                Ok(d) => self.voip.opus_decoder.push(d),
                Err(e) => {
                    self.print_out.push_str(&format!("VoIP: Error opus_decoder_create({i}) {e}\n"));
                    return;
                }
            }
            self.voip.ignore[i] = false;
            self.voip.gain[i] = 1.0;
        }
        let v = &mut self.voip;
        v.codec_initialized = true;
        v.mute_all = false;
        v.command_registered = true;
        v.pending_send_target = true;
        v.targets = [0xff; VOIP_TARGET_BYTES];
    }

    /// `CL_SystemInfoChanged`: the server speaks opus.
    pub(super) fn voip_system_info_changed(&mut self, systeminfo: &str) {
        self.voip.enabled = svgame::info_value(systeminfo, "sv_voipProtocol").eq_ignore_ascii_case("opus");
    }

    /// `CL_Disconnect`: dump any pending transmission, stop capturing (on
    /// the next `capture_voip`), free the codec and the "voip" command;
    /// not connected to a voip server anymore.
    pub(super) fn voip_disconnect(&mut self) {
        let v = &mut self.voip;
        v.outgoing_data_size = 0;
        let (capturing, demoplaying, cl_voip) = (v.capturing, v.demoplaying, v.cl_voip);
        *v = ClVoip { pending_stop: capturing, capturing, demoplaying, cl_voip, ..ClVoip::default() };
    }

    /// `CL_UpdateVoipIgnore`.
    fn update_voip_ignore(&mut self, idstr: &str, ignore: bool) {
        if idstr.as_bytes().first().is_some_and(|c| c.is_ascii_digit()) {
            let id = super::spawn::c_atoi(idstr);
            if (0..MAX_CLIENTS as i32).contains(&id) {
                self.voip.ignore[id as usize] = ignore;
                let _ = self.add_reliable_command(&format!("voip {} {id}", if ignore { "ignore" } else { "unignore" }), false);
                self.print_out.push_str(&format!("VoIP: {} ignoring player #{id}\n", if ignore { "Now" } else { "No longer" }));
                return;
            }
        }
        self.print_out.push_str("VoIP: invalid player ID#\n");
    }

    /// `CL_UpdateVoipGain`.
    fn update_voip_gain(&mut self, idstr: &str, gain: f32) {
        if idstr.as_bytes().first().is_some_and(|c| c.is_ascii_digit()) {
            let id = super::spawn::c_atoi(idstr);
            let gain = gain.max(0.0);
            if (0..MAX_CLIENTS as i32).contains(&id) {
                self.voip.gain[id as usize] = gain;
                self.print_out.push_str(&format!("VoIP: player #{id} gain now set to {gain:.6}\n"));
            }
        }
    }

    /// `CL_Voip_f`: voip [un]ignore <id> | [un]muteall | gain <id> [value].
    pub fn voip_f(&mut self, argv: &[String]) {
        let a = |i: usize| argv.get(i).cloned().unwrap_or_default();
        let reason = if self.state != ConnState::Active {
            Some("Not connected to a server")
        } else if !self.voip.codec_initialized {
            Some("Voip codec not initialized")
        } else if !self.voip.enabled {
            Some("Server doesn't support VoIP")
        } else {
            None
        };
        if let Some(r) = reason {
            self.print_out.push_str(&format!("VoIP: command ignored: {r}\n"));
            return;
        }
        match a(1).as_str() {
            "ignore" => self.update_voip_ignore(&a(2), true),
            "unignore" => self.update_voip_ignore(&a(2), false),
            "gain" => {
                if argv.len() > 3 {
                    self.update_voip_gain(&a(2), super::spawn::c_atof(&a(3)) as f32);
                } else if is_a_number(&a(2)) {
                    let id = super::spawn::c_atoi(&a(2));
                    if (0..MAX_CLIENTS as i32).contains(&id) {
                        self.print_out.push_str(&format!("VoIP: current gain for player #{id} is {:.6}\n", self.voip.gain[id as usize]));
                    } else {
                        self.print_out.push_str("VoIP: invalid player ID#\n");
                    }
                } else {
                    self.print_out.push_str("usage: voip gain <playerID#> [value]\n");
                }
            }
            "muteall" => {
                self.print_out.push_str("VoIP: muting incoming voice\n");
                let _ = self.add_reliable_command("voip muteall", false);
                self.voip.mute_all = true;
            }
            "unmuteall" => {
                self.print_out.push_str("VoIP: unmuting incoming voice\n");
                let _ = self.add_reliable_command("voip unmuteall", false);
                self.voip.mute_all = false;
            }
            _ => self.print_out.push_str("usage: voip [un]ignore <playerID#>\n       voip [un]muteall\n       voip gain <playerID#> [value]\n"),
        }
    }

    /// `CL_VoipNewGeneration`.
    fn voip_new_generation(&mut self) {
        let v = &mut self.voip;
        // don't have a zero generation so new clients won't match, and don't
        //  wrap to negative so MSG_ReadLong() doesn't "fail."
        v.outgoing_generation = v.outgoing_generation.wrapping_add(1);
        if v.outgoing_generation == 0 {
            v.outgoing_generation = 1;
        }
        v.power = 0.0;
        v.outgoing_sequence = 0;
        if let Some(e) = v.opus_encoder.as_mut() {
            e.reset();
        }
    }

    /// `CL_VoipParseTargets`: clc.voipTargets from cl_voipSendTarget
    /// ("all", "spatial", "attacker", "crosshair" and client numbers,
    /// comma or space separated; other words are skipped).
    fn voip_parse_targets(&mut self, target: &str, host: &dyn VoipHost) {
        self.voip.targets = [0; VOIP_TARGET_BYTES];
        self.voip.flags &= !(VOIP_SPATIAL as u8);
        let mut t = target.as_bytes();
        loop {
            while let Some(&c) = t.first() {
                if c == b',' || c == b' ' {
                    t = &t[1..];
                } else {
                    break;
                }
            }
            if t.is_empty() {
                break;
            }
            let val;
            if t[0].is_ascii_digit() {
                // strtol
                let n = t.iter().take_while(|c| c.is_ascii_digit()).count();
                val = std::str::from_utf8(&t[..n]).ok().and_then(|s| s.parse::<i64>().ok()).unwrap_or(i64::MAX).min(i32::MAX as i64) as i32;
                t = &t[n..];
            } else {
                if starts_ci(t, "all") {
                    self.voip.targets = [0xff; VOIP_TARGET_BYTES];
                    return;
                }
                if starts_ci(t, "spatial") {
                    self.voip.flags |= VOIP_SPATIAL as u8;
                    t = &t[7..];
                    continue;
                }
                if starts_ci(t, "attacker") {
                    val = host.last_attacker();
                    t = &t[8..];
                } else if starts_ci(t, "crosshair") {
                    val = host.crosshair_player();
                    t = &t[9..];
                } else {
                    while let Some(&c) = t.first() {
                        if c == b',' || c == b' ' {
                            break;
                        }
                        t = &t[1..];
                    }
                    continue;
                }
                if val < 0 {
                    continue;
                }
            }
            if !(0..MAX_CLIENTS as i32).contains(&val) {
                self.print_out.push_str(&format!("^3WARNING: VoIP target {val} is not a valid client number\n"));
                continue;
            }
            self.voip.targets[val as usize / 8] |= 1 << (val % 8);
        }
    }

    /// `CL_CaptureVoip`: record more audio from the hardware if required
    /// and encode it into Opus data for later transmission (`CL_Frame`,
    /// after `CL_SendCmd`).
    pub fn capture_voip(&mut self, host: &mut dyn VoipHost) {
        // CL_FirstSnapshot's cvar
        if std::mem::take(&mut self.voip.pending_send_target) {
            host.cvar_set("cl_voipSendTarget", "spatial");
        }
        // CL_Disconnect: cl_voipSend 0 and one more pass to clean up any state
        let disconnect_stop = std::mem::take(&mut self.voip.pending_stop);
        if disconnect_stop {
            if host.cvar_integer("cl_voipSend") != 0 {
                host.cvar_set("cl_voipSend", "0");
            }
            host.cvar_take_modified("cl_voipSend");
            if self.voip.capturing {
                host.stop_capture();
                host.master_gain(1.0);
                self.voip.capturing = false;
            }
            self.voip.power = 0.0;
            return;
        }
        let audio_mult = host.cvar_value("cl_voipCaptureMult");
        let use_vad = host.cvar_integer("cl_voipUseVAD") != 0;
        let mut initial_frame = false;
        let mut final_frame = false;
        // If your data rate is too low, you'll get Connection Interrupted warnings
        //  when VoIP packets arrive, even if you have a broadband connection.
        let voip_mod = host.cvar_take_modified("cl_voip");
        let rate_mod = host.cvar_take_modified("rate");
        if voip_mod || rate_mod {
            if host.cvar_integer("cl_voip") != 0 && host.cvar_integer("rate") < 25000 {
                self.print_out.push_str("^3Your network rate is too slow for VoIP.\n");
                self.print_out.push_str("Set 'Data Rate' to 'LAN/Cable/xDSL' in 'Setup/System/Network'.\n");
                self.print_out.push_str("Until then, VoIP is disabled.\n");
                host.cvar_set("cl_voip", "0");
            }
            let p = client_voip_protocol(host.cvar_integer("cl_voip"));
            host.cvar_set("cl_voipProtocol", p);
            host.cvar_take_modified("cl_voip");
            host.cvar_take_modified("rate");
        }
        self.voip.cl_voip = host.cvar_integer("cl_voip") != 0;
        if !self.voip.codec_initialized {
            return; // just in case this gets called at a bad time.
        }
        if self.voip.outgoing_data_size > 0 {
            return; // packet is pending transmission, don't record more yet.
        }
        if host.cvar_take_modified("cl_voipUseVAD") {
            host.cvar_set("cl_voipSend", if use_vad { "1" } else { "0" });
        }
        if use_vad && host.cvar_integer("cl_voipSend") == 0 {
            host.cvar_set("cl_voipSend", "1"); // lots of things reset this.
        }
        if host.cvar_take_modified("cl_voipSend") {
            let reason = if self.state != ConnState::Active {
                Some("not in a game") // not connected to a server.
            } else if !self.voip.enabled {
                Some("this server has no voice chat") // server doesn't support VoIP.
            } else if self.voip.demoplaying {
                Some("a demo is playing") // playing back a demo.
            } else if host.cvar_integer("cl_voip") == 0 {
                Some("voice chat is off in the options") // client has VoIP support disabled.
            } else if audio_mult == 0.0 {
                Some("the microphone volume is 0") // basically silenced incoming audio.
            } else {
                None
            };
            let dont_capture = reason.is_some();
            if dont_capture && host.cvar_integer("cl_voipSend") != 0 {
                self.voip.refused = reason;
            }
            if dont_capture {
                host.cvar_set("cl_voipSend", "0");
                host.cvar_take_modified("cl_voipSend");
                return;
            }
            if host.cvar_integer("cl_voipSend") != 0 {
                initial_frame = true;
            } else {
                final_frame = true;
            }
        }
        // try to get more audio data from the sound card...
        if initial_frame {
            host.master_gain(host.cvar_value("cl_voipGainDuringCapture").clamp(0.0, 1.0));
            host.start_capture();
            self.voip.capturing = true;
            self.voip_new_generation();
            let target = host.cvar_string("cl_voipSendTarget");
            self.voip_parse_targets(&target, host);
        }
        if host.cvar_integer("cl_voipSend") != 0 || final_frame {
            // user wants to capture audio?
            let mut samples = host.available_capture_samples();
            let packet_samples = if final_frame { VOIP_MAX_FRAME_SAMPLES } else { VOIP_MAX_PACKET_SAMPLES };
            // enough data buffered in audio hardware to process yet?
            if samples >= packet_samples {
                samples = samples.min(VOIP_MAX_PACKET_SAMPLES);
                samples -= samples % VOIP_MAX_FRAME_SAMPLES;
                if ![120, 240, 480, 960, 1920, 2880].contains(&samples) {
                    self.print_out.push_str(&format!("Voip: bad number of samples {samples}\n"));
                    return;
                }
                let voip_frames = samples / VOIP_MAX_FRAME_SAMPLES;
                // audio capture is always MONO16.
                let mut sampbuffer = host.capture(samples);
                sampbuffer.resize(samples, 0);
                // check the "power" of this packet...
                let mut voip_power = 0.0f32;
                for s in sampbuffer.iter_mut() {
                    let flsamp = *s as f32;
                    let a = flsamp.abs();
                    voip_power += a * a;
                    *s = (flsamp * audio_mult) as i32 as i16;
                }
                // encode raw audio samples into Opus data...
                let v = &mut self.voip;
                let mut bytes = v.opus_encoder.as_mut().map(|e| e.encode(&sampbuffer, &mut v.outgoing_data)).unwrap_or(0);
                if bytes <= 0 {
                    bytes = 0;
                }
                v.power = (voip_power / (32768.0 * 32768.0 * samples as f32)) * 100.0;
                if use_vad && v.power < host.cvar_value("cl_voipVADThreshold") {
                    self.voip_new_generation(); // no "talk" for at least 1/4 second.
                } else {
                    v.outgoing_data_size = bytes as usize;
                    v.outgoing_data_frames = voip_frames as i32;
                }
            }
        }
        // User requested we stop recording, and we've now processed the last of
        //  any previously-buffered data. Pause the capture device, etc.
        if final_frame {
            host.stop_capture();
            self.voip.capturing = false;
            host.master_gain(1.0);
            self.voip.power = 0.0; // force this value so it doesn't linger.
        }
    }

    /// `CL_WritePacket`'s VoIP block (before the usercmds).
    pub(super) fn write_voip(&mut self, buf: &mut Msg) {
        let v = &mut self.voip;
        if v.outgoing_data_size == 0 {
            return;
        }
        if v.flags as i32 & VOIP_SPATIAL != 0 || is_voip_target(&v.targets, -1) {
            buf.write_byte(clc::VOIP_OPUS);
            buf.write_byte(v.outgoing_generation as i32);
            buf.write_long(v.outgoing_sequence);
            buf.write_byte(v.outgoing_data_frames);
            buf.write_data(&v.targets);
            buf.write_byte(v.flags as i32);
            buf.write_short(v.outgoing_data_size as i32);
            buf.write_data(&v.outgoing_data[..v.outgoing_data_size]);
            v.outgoing_sequence += v.outgoing_data_frames;
        }
        // else: we have data, but no targets. Silently discard all data
        v.outgoing_data_size = 0;
        v.outgoing_data_frames = 0;
    }

    /// `CL_ShouldIgnoreVoipSender` (cl_voip is the host's cvar, `voip_on`).
    fn should_ignore_voip_sender(&self, sender: usize, voip_on: bool) -> bool {
        if !voip_on {
            true // VoIP is disabled.
        } else if sender as i32 == self.client_num && !self.voip.demoplaying {
            true // ignore own voice (unless playing back a demo).
        } else if self.voip.mute_all {
            true // all channels are muted with extreme prejudice.
        } else if self.voip.ignore[sender] {
            true // just ignoring this guy.
        } else {
            self.voip.gain[sender] == 0.0 // too quiet to play.
        }
    }

    /// `CL_PlayVoip`: play raw data.
    fn play_voip(&mut self, sender: usize, samples: &[i16], flags: i32) {
        if flags & VOIP_DIRECT != 0 {
            self.voip.playback.push(VoipPlayback { stream: sender + VOIP_STREAM_BASE, sender, samples: samples.to_vec(), volume: self.voip.gain[sender], entity: -1 });
        }
        if flags & VOIP_SPATIAL != 0 {
            self.voip.playback.push(VoipPlayback { stream: sender + MAX_CLIENTS + VOIP_STREAM_BASE, sender, samples: samples.to_vec(), volume: 1.0, entity: sender as i32 });
        }
    }

    /// `CL_ParseVoip`: a VoIP message has been received from the server.
    pub(super) fn parse_voip(&mut self, m: &mut Msg, ignore_data: bool) {
        let sender = m.read_short();
        let generation = m.read_byte();
        let sequence = m.read_long();
        let frames = m.read_byte();
        let packetsize = m.read_short();
        let flags = m.read_bits(VOIP_FLAGCNT);
        const ENCODED: usize = 4000;
        if sender < 0 || generation < 0 || sequence < 0 || frames < 0 || packetsize < 0 {
            return; // short/invalid packet, bail.
        }
        if packetsize as usize > ENCODED {
            // overlarge packet?
            let mut bytesleft = packetsize as usize;
            while bytesleft > 0 {
                let br = bytesleft.min(ENCODED);
                m.read_data(br);
                bytesleft -= br;
            }
            return; // overlarge packet, bail.
        }
        let encoded = m.read_data(packetsize as usize);
        if ignore_data {
            return; // just ignore legacy speex voip data
        } else if !self.voip.codec_initialized {
            return; // can't handle VoIP without libopus!
        } else if sender as usize >= MAX_CLIENTS {
            return; // bogus sender.
        } else if self.should_ignore_voip_sender(sender as usize, self.voip.cl_voip) {
            return; // Channel is muted, bail.
        }
        let sender = sender as usize;
        // static short decoded[VOIP_MAX_PACKET_SAMPLES*4]
        let cap = VOIP_MAX_PACKET_SAMPLES * 4;
        let mut decoded = vec![0i16; cap];
        let mut written = 0usize;
        let mut seqdiff = sequence - self.voip.incoming_sequence[sender];
        // This is a new "generation" ... a new recording started, reset the bits.
        if generation != self.voip.incoming_generation[sender] as i32 {
            self.voip.opus_decoder[sender].reset();
            self.voip.incoming_generation[sender] = generation as u8;
            seqdiff = 0;
        } else if seqdiff < 0 {
            // we're ahead of the sequence?!
            // This shouldn't happen unless the packet is corrupted or something.
            // reset the decoder just in case.
            self.voip.opus_decoder[sender].reset();
            seqdiff = 0;
        } else if seqdiff as usize * VOIP_MAX_PACKET_SAMPLES * 2 >= cap * 2 {
            // dropped more than we can handle? just start over.
            self.voip.opus_decoder[sender].reset();
            seqdiff = 0;
        }
        // tell opus that we're missing frames...
        for _ in 0..seqdiff {
            if written + VOIP_MAX_PACKET_SAMPLES > cap {
                break;
            }
            let n = self.voip.opus_decoder[sender].decode(None, &mut decoded[written..written + VOIP_MAX_PACKET_SAMPLES]);
            if n <= 0 {
                continue;
            }
            written += n as usize;
        }
        let mut num_samples = self.voip.opus_decoder[sender].decode(Some(&encoded), &mut decoded[written..]);
        if num_samples <= 0 {
            num_samples = 0;
        }
        written += num_samples as usize;
        if written > 0 {
            let d = decoded[..written].to_vec();
            self.play_voip(sender, &d, flags);
        }
        self.voip.incoming_sequence[sender] = sequence + frames;
    }

    /// `SCR_DrawVoipMeter`'s text ("VoIP: [*****     ]"), None when it is
    /// not drawn (meter off, not recording, not in a VoIP game).
    pub fn voip_meter(&self, show_meter: bool, voip_send: bool, voip_on: bool) -> Option<String> {
        if !show_meter || !voip_send || self.state != ConnState::Active || !self.voip.enabled || self.voip.demoplaying || !voip_on {
            return None;
        }
        let limit = ((self.voip.power * 10.0) as i32).min(10).max(0) as usize;
        Some(format!("VoIP: [{}{}]", "*".repeat(limit), " ".repeat(10 - limit)))
    }
}

/// `Q_isanumber`.
fn is_a_number(s: &str) -> bool {
    !s.is_empty() && s.trim().parse::<f64>().is_ok()
}

/// `IN_VoipRecordDown` / `IN_VoipRecordUp`: the cl_voipSend value of
/// +voiprecord / -voiprecord.
pub fn voip_record(down: bool) -> &'static str {
    if down {
        "1"
    } else {
        "0"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voip_targets_bits() {
        let mut t = [0u8; VOIP_TARGET_BYTES];
        assert!(!is_voip_target(&t, -1));
        t[1] = 1 << 2;
        assert!(is_voip_target(&t, 10));
        assert!(!is_voip_target(&t, 11));
        assert!(is_voip_target(&t, -1));
        assert!(!is_voip_target(&t, 64));
    }

    #[test]
    fn server_voip_command() {
        let mut v = SvClientVoip::default();
        v.voip_f(&["voip".into(), "ignore".into(), "5".into()]);
        assert!(v.ignore_voip_from_client[5]);
        v.voip_f(&["voip".into(), "unignore".into(), "5".into()]);
        assert!(!v.ignore_voip_from_client[5]);
        v.voip_f(&["voip".into(), "ignore".into(), "x5".into()]);
        v.voip_f(&["voip".into(), "muteall".into()]);
        assert!(v.mute_all_voip);
        v.userinfo_changed("\\name\\a\\cl_voipProtocol\\opus");
        assert!(v.has_voip);
        v.userinfo_changed("\\name\\a");
        assert!(!v.has_voip);
    }

    /// A voip block whose size short is negative: `as usize` made it about
    /// 2^64, and the "overlarge packet" skip loop then never ended (any
    /// client could hang the server; the speex path runs with voip off).
    #[test]
    fn server_voip_negative_size_is_dropped() {
        use crate::gen::maps::MapId;
        use crate::kernel::sv::ServerCvars;
        use crate::kernel::world::World;
        let mut sv = Server::spawn(World::new(MapId::Arena), "arena", ServerCvars::default());
        for size in [-1, -32768] {
            let mut m = Msg::new(64);
            m.write_byte(0); // generation
            m.write_long(1); // sequence
            m.write_byte(1); // frames
            m.write_data(&[0xff; VOIP_TARGET_BYTES]);
            m.write_byte(0); // flags
            m.write_short(size);
            m.begin_reading();
            let start = std::time::Instant::now();
            for ignore_data in [true, false] {
                let mut r = Msg::from_bytes(&m.data[..m.cursize]);
                r.begin_reading();
                sv.user_voip(0, &mut r, ignore_data);
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(1), "size {size}");
        }
    }
}
