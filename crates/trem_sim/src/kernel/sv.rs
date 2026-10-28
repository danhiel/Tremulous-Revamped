//! Kernel `sv` â€” the server engine of tremulous-legacy (dec033):
//! server/sv_main.c (connectionless packets `getstatus` / `getinfo` /
//! `getchallenge` / `connect` / `rcon`, the leaky-bucket rate limits,
//! `SV_PacketEvent`, `SV_CalcPings`, `SV_CheckTimeouts`, `SV_Frame`,
//! `SV_RateMsec`, `SV_MasterHeartbeat`), server/sv_client.c
//! (`SV_GetChallenge`, `SV_DirectConnect`, `SV_DropClient`,
//! `SV_SendClientGameState`, `SV_ClientEnterWorld`, the ucmds incl. the
//! UDP download, `SV_ExecuteClientMessage`, `SV_UserMove`), sv_snapshot.c
//! (`SV_BuildClientSnapshot` with PVS / area culling, `SV_EmitPacketEntities`
//! against the client's acknowledged frame, `SV_SendClientMessages`),
//! sv_init.c (configstrings, baselines, `SV_SpawnServer` with its
//! `FS_Restart( sv.checksumFeed )`, `SV_TouchCGame` and the sv_paks /
//! sv_referencedPaks systeminfo of a pure server), the pure check
//! (`SV_VerifyPaks_f`, `SV_ResetPureClient_f`, `SV_UserMove`'s
//! pureAuthentic / gotCP gate; dec037), the referenced-pak check of
//! `SV_WriteDownloadToClient` and sv_net_chan.c (the transmit queue).
//!
//! The game module is the trem_sim `World` the server owns; svgame.rs packs
//! its state. Time: `svs_time` is the server's own clock, `World::level.time`
//! is `sv.time`.

use super::files::{self, PakSet};
use super::msg::{self, EntityState, Msg, GENTITYNUM_BITS, MAX_GENTITIES, MAX_MSGLEN};
use super::netchan::{self, Net, NetAdr, NetAdrType, NetSrc, Netchan};
use super::qmath::Vec3;
use super::svgame::{self, cs};
use super::types::*;
use super::world::{ConnectInfo, World, MAX_CLIENTS};
use crate::gen::enums::Team;
use std::collections::{HashMap, VecDeque};

/// `PROTOCOL_VERSION`.
pub const PROTOCOL_VERSION: i32 = 71;
/// `sv_allowDownload` / `cl_allowDownload` flags (qcommon.h `DLF_*`).
pub const DLF_ENABLE: i32 = 1;
pub const DLF_NO_REDIRECT: i32 = 2;
pub const DLF_NO_UDP: i32 = 4;
pub const DLF_NO_DISCONNECT: i32 = 8;
/// `PACKET_BACKUP` / `PACKET_MASK`.
pub const PACKET_BACKUP: usize = 32;
pub const PACKET_MASK: usize = PACKET_BACKUP - 1;
/// `MAX_RELIABLE_COMMANDS`.
pub const MAX_RELIABLE_COMMANDS: usize = 128;
/// `MAX_PACKET_USERCMDS`.
pub const MAX_PACKET_USERCMDS: i32 = 32;
/// `MAX_SNAPSHOT_ENTITIES`.
pub const MAX_SNAPSHOT_ENTITIES: usize = 256;
/// `MAX_CHALLENGES` / `MAX_CHALLENGES_MULTI`.
pub const MAX_CHALLENGES: usize = 2048;
pub const MAX_CHALLENGES_MULTI: usize = MAX_CHALLENGES / 2;
/// `MAX_DOWNLOAD_WINDOW` / `MAX_DOWNLOAD_BLKSIZE`. The window stays 48
/// (each block costs the client a reliable "nextdl", and a 1.1 client holds
/// 64); the blocks are 8 KB instead of 1 KB, which every client reads (a
/// chunk may be up to MAX_MSGLEN) and the netchan fragments. With no rate cap
/// a download moves a whole 384 KB window per round trip.
pub const MAX_DOWNLOAD_WINDOW: usize = 48;
pub const MAX_DOWNLOAD_BLKSIZE: usize = 8192;
/// Resend the window when no block was acknowledged for this long (+ the
/// client's ping); the original waited 1000 ms.
const DOWNLOAD_RESEND_MSEC: i32 = 250;
/// `MAX_MASTER_SERVERS`.
pub const MAX_MASTER_SERVERS: usize = 5;
/// `HEARTBEAT_MSEC`.
pub const HEARTBEAT_MSEC: i32 = 300 * 1000;
/// `HEARTBEAT_FOR_MASTER` / `com_gamename`.
pub const GAMENAME: &str = "Tremulous";
/// `MASTER_SERVER_NAME`.
pub const MASTER_SERVER_NAME: &str = "master.tremulous.net";
/// `SNAPFLAG_*`.
pub const SNAPFLAG_RATE_DELAYED: i32 = 1;
pub const SNAPFLAG_NOT_ACTIVE: i32 = 2;
pub const SNAPFLAG_SERVERCOUNT: i32 = 4;
/// `MAX_MAP_AREA_BYTES`.
pub const MAX_MAP_AREA_BYTES: usize = 32;
/// `UDPIP_HEADER_SIZE` / `UDPIP6_HEADER_SIZE`.
const UDPIP_HEADER_SIZE: usize = 28;
const UDPIP6_HEADER_SIZE: usize = 48;

/// `svc_ops_e`.
pub mod svc {
    pub const BAD: i32 = 0;
    pub const NOP: i32 = 1;
    pub const GAMESTATE: i32 = 2;
    pub const CONFIGSTRING: i32 = 3;
    pub const BASELINE: i32 = 4;
    pub const SERVER_COMMAND: i32 = 5;
    pub const DOWNLOAD: i32 = 6;
    pub const SNAPSHOT: i32 = 7;
    pub const EOF: i32 = 8;
    pub const VOIP_SPEEX: i32 = 9;
    pub const VOIP_OPUS: i32 = 10;
}

/// `clc_ops_e`.
pub mod clc {
    pub const BAD: i32 = 0;
    pub const NOP: i32 = 1;
    pub const MOVE: i32 = 2;
    pub const MOVE_NO_DELTA: i32 = 3;
    pub const CLIENT_COMMAND: i32 = 4;
    pub const EOF: i32 = 5;
    pub const VOIP_SPEEX: i32 = 6;
    pub const VOIP_OPUS: i32 = 7;
}

/// `clientState_t`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum ClientState {
    /// can be reused for a new connection
    #[default]
    Free,
    /// client has been disconnected, but don't reuse connection for a couple seconds
    Zombie,
    /// has been assigned to a client_t, but no gamestate yet
    Connected,
    /// gamestate has been sent, but client hasn't sent a usercmd
    Primed,
    /// client is fully in game
    Active,
}

/// `clientSnapshot_t` (the entity states kept in the frame itself instead
/// of svs.snapshotEntities' ring).
#[derive(Clone, Debug, Default)]
pub struct ClientSnapshot {
    pub areabytes: usize,
    pub areabits: [u8; MAX_MAP_AREA_BYTES],
    pub ps: PlayerState,
    pub entities: Vec<EntityState>,
    /// time the message was transmitted
    pub message_sent: i32,
    /// time the message was acked
    pub message_acked: i32,
    /// used to rate drop packets
    pub message_size: usize,
}

/// The UDP download of a client (`download*` members of client_t).
#[derive(Clone, Debug, Default)]
pub struct Download {
    pub name: String,
    pub data: Option<Vec<u8>>,
    pub size: i32,
    pub count: i32,
    pub current_block: usize,
    pub client_block: usize,
    pub xmit_block: usize,
    pub blocks: Vec<Vec<u8>>,
    pub eof: bool,
    pub send_time: i32,
    /// per window slot: when the block was last sent
    pub sent_at: Vec<i32>,
    /// (netchan sequence, time) of the latest download messages, and the
    /// highest sequence the client acknowledged
    pub msg_sent: Vec<(i32, i32)>,
    pub rtt_acked: i32,
    /// smoothed round trip from a download message to the client packet
    /// acknowledging it, 0 before the first sample
    pub srtt: i32,
}

/// How many download messages `Download::msg_sent` remembers.
const DOWNLOAD_MSG_RING: usize = 256;

/// `client_t`.
#[derive(Clone, Debug)]
pub struct SvClient {
    pub state: ClientState,
    pub userinfo: String,
    pub name: String,
    pub reliable_commands: Vec<String>,
    /// last added reliable message, not necesarily sent or acknowledged yet
    pub reliable_sequence: i32,
    /// last acknowledged reliable message
    pub reliable_acknowledge: i32,
    /// last sent reliable message, not necesarily acknowledged yet
    pub reliable_sent: i32,
    pub message_acknowledge: i32,
    /// netchan->outgoingSequence of gamestate
    pub gamestate_message_num: i32,
    pub challenge: i32,
    pub last_usercmd: UserCmd,
    /// reliable client message sequence
    pub last_client_command: i32,
    pub last_client_command_string: String,
    /// frame last client usercmd message
    pub delta_message: i32,
    pub next_reliable_time: i32,
    /// svs.time when packet was last received
    pub last_packet_time: i32,
    /// svs.time when connection started
    pub last_connect_time: i32,
    pub last_snapshot_time: i32,
    pub rate_delayed: bool,
    pub timeout_count: i32,
    pub frames: Vec<ClientSnapshot>,
    pub ping: i32,
    /// bytes / second
    pub rate: i32,
    /// requests a snapshot every snapshotMsec unless rate choked
    pub snapshot_msec: i32,
    pub download: Download,
    pub netchan: Netchan,
    /// messages waiting for the netchan's fragments (sv_net_chan.c queue)
    pub netchan_queue: VecDeque<Vec<u8>>,
    /// configstrings changed while CS_PRIMED
    pub cs_updated: Vec<bool>,
    /// sv.time of the previous map while the new gamestate is not acked
    pub old_server_time: i32,
    /// `pureAuthentic` / `gotCP`: the client's "cp" passed SV_VerifyPaks_f
    pub pure_authentic: bool,
    pub got_cp: bool,
    /// hasVoip, the voip ignores and the voipPacket queue (voip.rs; wt-b VoIP)
    pub voip: super::voip::SvClientVoip,
}

impl SvClient {
    fn new(netchan: Netchan) -> Self {
        SvClient {
            state: ClientState::Free,
            userinfo: String::new(),
            name: String::new(),
            reliable_commands: vec![String::new(); MAX_RELIABLE_COMMANDS],
            reliable_sequence: 0,
            reliable_acknowledge: 0,
            reliable_sent: 0,
            message_acknowledge: 0,
            gamestate_message_num: -1,
            challenge: 0,
            last_usercmd: UserCmd::default(),
            last_client_command: 0,
            last_client_command_string: String::new(),
            delta_message: -1,
            next_reliable_time: 0,
            last_packet_time: 0,
            last_connect_time: 0,
            last_snapshot_time: 0,
            rate_delayed: false,
            timeout_count: 0,
            frames: vec![ClientSnapshot::default(); PACKET_BACKUP],
            ping: 0,
            rate: 25000,
            snapshot_msec: 50,
            download: Download::default(),
            netchan,
            netchan_queue: VecDeque::new(),
            cs_updated: vec![false; cs::MAX_CONFIGSTRINGS],
            old_server_time: 0,
            pure_authentic: false,
            got_cp: false,
            voip: super::voip::SvClientVoip::default(),
        }
    }
}

/// `challenge_t`.
#[derive(Clone, Debug, Default)]
pub struct Challenge {
    pub adr: NetAdr,
    pub challenge: i32,
    pub client_challenge: i32,
    /// time the last packet was sent to the autherize server
    pub time: i32,
    /// time the challenge response was sent to client
    pub ping_time: i32,
    /// time the adr was first used, for authorize timeout checks
    pub first_time: i32,
    pub was_refused: bool,
    pub connected: bool,
}

/// `MAX_BUCKETS`: the most addresses the rate limits track.
const MAX_BUCKETS: usize = 16384;
/// A bucket idle this long has drained at every burst / period used (10 x 1000).
const BUCKET_IDLE_MSEC: i32 = 10 * 1000;

/// `leakyBucket_t` (keyed by address instead of the C's hash chains).
#[derive(Clone, Copy, Debug, Default)]
struct Bucket {
    last_time: i32,
    burst: i32,
}

/// The server cvars (sv_init.c `SV_Init` defaults; the port runs the game
/// at 16 ms frames, sv_fps 60).
#[derive(Clone, Debug)]
pub struct ServerCvars {
    pub hostname: String,
    pub maxclients: i32,
    pub private_clients: i32,
    pub private_password: String,
    pub rcon_password: String,
    pub fps: i32,
    pub timeout: i32,
    pub zombietime: i32,
    pub reconnect_limit: i32,
    pub allow_download: i32,
    /// `sv_pure` (default 1 as in the C; a server without pk3s is not pure, dec037)
    pub pure: i32,
    /// `sv_dlURL` (serverinfo only: the port has no cURL)
    pub dl_url: String,
    pub min_rate: i32,
    pub max_rate: i32,
    pub min_ping: i32,
    pub max_ping: i32,
    pub lan_force_rate: bool,
    pub pad_packets: i32,
    /// dedicated 0 (listen), 1 (LAN), 2 (Internet: heartbeats)
    pub dedicated: i32,
    pub masters: [String; MAX_MASTER_SERVERS],
    pub cheats: bool,
    pub motd: String,
    /// the worldspawn "message" (CS_MESSAGE)
    pub message: String,
    /// other CVAR_SERVERINFO cvars (timelimit, g_* the ui shows)
    pub serverinfo_extra: Vec<(String, String)>,
    /// sv_voip (wt-b VoIP; sv_voipProtocol is "opus" while it is on)
    pub voip: i32,
}

impl Default for ServerCvars {
    fn default() -> Self {
        ServerCvars {
            hostname: "noname".into(),
            maxclients: 8,
            private_clients: 0,
            private_password: String::new(),
            rcon_password: String::new(),
            fps: 60,
            timeout: 200,
            zombietime: 2,
            reconnect_limit: 3,
            allow_download: 0,
            pure: 1,
            dl_url: String::new(),
            min_rate: 0,
            max_rate: 0,
            min_ping: 0,
            max_ping: 0,
            lan_force_rate: true,
            pad_packets: 0,
            dedicated: 0,
            masters: [MASTER_SERVER_NAME.into(), "master.ioquake3.org".into(), String::new(), String::new(), String::new()],
            cheats: false,
            motd: String::new(),
            message: String::new(),
            serverinfo_extra: Vec::new(),
            voip: 1,
        }
    }
}

/// A temp entity the server made from the World's events or zaps.
#[derive(Clone, Debug)]
struct TempEnt {
    state: EntityState,
    /// sv.time it stops being sent
    expire: i32,
    broadcast: bool,
    /// SVF_NOTSINGLECLIENT: not sent to this client (its predicted event)
    not_client: Option<usize>,
}

/// One running server (`serverStatic_t` + `server_t`).
pub struct Server {
    pub world: World,
    pub cvars: ServerCvars,
    pub map_name: String,
    pub clients: Vec<SvClient>,
    pub challenges: Vec<Challenge>,
    /// svs.time
    pub time: i32,
    pub time_residual: i32,
    pub server_id: i32,
    pub restarted_server_id: i32,
    pub checksum_feed: i32,
    /// `sv.checksumFeedServerId`: older "cp" commands are ignored
    pub checksum_feed_server_id: i32,
    /// the search path (`fs_searchpaths`): pure checksums and referenced
    /// paks (None: no pk3s, the sheet maps)
    pub fs: Option<PakSet>,
    pub snap_flag_server_bit: i32,
    pub configstrings: Vec<String>,
    pub baselines: Vec<Option<EntityState>>,
    next_heartbeat_time: i32,
    buckets: HashMap<[u8; 16], Bucket>,
    outbound_bucket: Bucket,
    rcon_bucket: Bucket,
    temps: HashMap<i32, TempEnt>,
    /// numbers recently used by temp entities: reusable after this sv.time
    temp_free_at: HashMap<i32, i32>,
    zap_nums: HashMap<(usize, usize), i32>,
    sent: svgame::Sent,
    /// last teamplay info time (CheckTeamStatus)
    last_team_status: i32,
    /// server console output (Com_Printf)
    pub console: String,
    /// rcon lines with a good password, for the host to run: (from, line)
    pub rcon_queue: Vec<(NetAdr, String)>,
    /// `FS_SV_FOpenFileRead` for downloads (the host's pk3 folders)
    pub file_source: Option<Box<dyn FnMut(&str) -> Option<Vec<u8>>>>,
    /// masters resolved by a background thread (heartbeats)
    master_adrs: Vec<Option<NetAdr>>,
    master_rx: Option<std::sync::mpsc::Receiver<(usize, Option<NetAdr>)>>,
    rng: u32,
}

fn now_ms() -> i32 {
    use std::sync::OnceLock;
    static T0: OnceLock<std::time::Instant> = OnceLock::new();
    T0.get_or_init(std::time::Instant::now).elapsed().as_millis() as i32
}

impl Server {
    /// `SV_SpawnServer` on a loaded World (the game's `G_InitGame` has run).
    pub fn spawn(world: World, map_name: &str, cvars: ServerCvars) -> Server {
        Self::spawn_fs(world, map_name, cvars, None)
    }

    /// `SV_SpawnServer` with the host's pk3 search path.
    pub fn spawn_fs(world: World, map_name: &str, cvars: ServerCvars, fs: Option<PakSet>) -> Server {
        let mut rng = (now_ms() as u32 ^ 0x9e37_79b9).wrapping_mul(2654435761) | 1;
        let mut rand = || {
            rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
            (rng >> 1) as i32
        };
        let server_id = rand() & 0x7fff_ffff;
        let checksum_feed = (rand() << 16) ^ rand() ^ now_ms();
        let maxclients = cvars.maxclients.clamp(1, MAX_CLIENTS as i32) as usize;
        let mut sv = Server {
            world,
            cvars,
            map_name: map_name.to_string(),
            clients: (0..maxclients).map(|_| SvClient::new(Netchan::setup(NetSrc::Server, NetAdr::default(), 0, 0))).collect(),
            challenges: vec![Challenge::default(); MAX_CHALLENGES],
            time: 0,
            time_residual: 0,
            server_id,
            restarted_server_id: server_id,
            checksum_feed,
            checksum_feed_server_id: server_id,
            fs,
            snap_flag_server_bit: 0,
            configstrings: vec![String::new(); cs::MAX_CONFIGSTRINGS],
            baselines: vec![None; MAX_GENTITIES as usize],
            next_heartbeat_time: -9999,
            buckets: HashMap::new(),
            outbound_bucket: Bucket::default(),
            rcon_bucket: Bucket::default(),
            temps: HashMap::new(),
            temp_free_at: HashMap::new(),
            zap_nums: HashMap::new(),
            sent: Default::default(),
            last_team_status: 0,
            console: String::new(),
            rcon_queue: Vec::new(),
            file_source: None,
            master_adrs: vec![None; MAX_MASTER_SERVERS],
            master_rx: None,
            rng,
        };
        sv.world.level.dedicated = sv.cvars.dedicated != 0;
        // run a few frames to allow everything to settle
        for _ in 0..3 {
            sv.world.frame(sv.frame_msec());
        }
        sv.fs_spawn();
        sv.flush_game_output();
        sv.update_configstrings();
        sv.create_baselines();
        sv.console.push_str(&format!("-----------------------------------\nServer {} spawned (serverid {})\n", sv.map_name, sv.server_id));
        sv
    }

    /// `SV_SpawnServer` for the next map: the connected clients are carried
    /// over (`ClientConnect( firstTime = qfalse )`) and get the new gamestate.
    pub fn change_map(&mut self, world: World, map_name: &str) {
        let old_time = self.world.level.time;
        let infos: Vec<Option<ConnectInfo>> = (0..self.clients.len()).map(|i| (self.clients[i].state >= ClientState::Connected).then(|| self.connect_info(i))).collect();
        self.world = world;
        self.map_name = map_name.to_string();
        self.snap_flag_server_bit ^= SNAPFLAG_SERVERCOUNT;
        // serverid should be different each time, and go up (com_frameTime):
        // "cp" commands of an older one are ignored
        self.server_id = (self.server_id.wrapping_add(1 + (self.rand() & 0xffff)) & 0x7fff_ffff).max(1);
        self.restarted_server_id = self.server_id;
        self.checksum_feed = (self.rand() << 16) ^ self.rand() ^ now_ms();
        self.checksum_feed_server_id = self.server_id;
        self.configstrings = vec![String::new(); cs::MAX_CONFIGSTRINGS];
        self.baselines = vec![None; MAX_GENTITIES as usize];
        self.temps.clear();
        self.temp_free_at.clear();
        self.zap_nums.clear();
        self.sent = Default::default();
        self.world.level.dedicated = self.cvars.dedicated != 0;
        for _ in 0..3 {
            self.world.frame(self.frame_msec());
        }
        for (i, info) in infos.into_iter().enumerate() {
            let Some(info) = info else { continue };
            match self.world.connect_ex(Some(i), &info, false) {
                Ok(_) => {
                    let c = &mut self.clients[i];
                    c.state = ClientState::Connected;
                    c.old_server_time = old_time;
                    c.cs_updated.iter_mut().for_each(|x| *x = false);
                }
                Err(reason) => self.drop_client(i, &reason),
            }
        }
        self.fs_spawn();
        self.flush_game_output();
        self.update_configstrings();
        self.create_baselines();
    }

    /// SV_SpawnServer's filesystem part: `FS_Restart( sv.checksumFeed )`
    /// (references cleared), the map's BSP and the game module referenced
    /// (`CM_LoadMap`, `VM_Create( "qagame" )`), and for a pure dedicated
    /// server `SV_TouchCGame`.
    fn fs_spawn(&mut self) {
        let feed = self.checksum_feed;
        let (pure, dedicated) = (self.cvars.pure != 0, self.cvars.dedicated != 0);
        let map = format!("maps/{}.bsp", self.map_name);
        if let Some(fs) = self.fs.as_mut() {
            fs.restart(feed);
            fs.reference(&map);
            fs.reference_module(&files::GAME_MODULE, 0);
            if pure && dedicated {
                fs.reference_module(&files::CGAME_MODULE, 0);
            }
            if pure && fs.paks.is_empty() {
                self.console.push_str("WARNING: sv_pure set but no PK3 files loaded\n");
            }
        }
    }

    /// sv_pure with pk3s loaded: clients must pass SV_VerifyPaks_f.
    pub fn pure_active(&self) -> bool {
        self.cvars.pure != 0 && self.fs.as_ref().is_some_and(|f| !f.paks.is_empty())
    }

    fn rand(&mut self) -> i32 {
        self.rng = self.rng.wrapping_mul(1103515245).wrapping_add(12345);
        (self.rng >> 1) as i32
    }

    /// 1000 / sv_fps.
    pub fn frame_msec(&self) -> i32 {
        (1000 / self.cvars.fps.max(1)).max(1)
    }

    fn connect_info(&self, i: usize) -> ConnectInfo {
        let ui = &self.clients[i].userinfo;
        ConnectInfo {
            name: svgame::info_value(ui, "name"),
            guid: svgame::info_value(ui, "cl_guid"),
            ip: svgame::info_value(ui, "ip"),
            password: svgame::info_value(ui, "password"),
            use_unlagged: { let v = svgame::info_value(ui, "cg_unlagged"); v.is_empty() || super::spawn::c_atoi(&v) != 0 },
            team_info: super::spawn::c_atoi(&svgame::info_value(ui, "teamoverlay")) != 0,
        }
    }

    // ============================================================ commands

    /// `SV_AddServerCommand`.
    fn add_server_command(&mut self, i: usize, cmd: &str) {
        let c = &mut self.clients[i];
        if c.state < ClientState::Primed {
            return;
        }
        c.reliable_sequence += 1;
        // if we would be losing an old command that hasn't been acknowledged,
        // we must drop the connection
        if c.reliable_sequence - c.reliable_acknowledge == MAX_RELIABLE_COMMANDS as i32 + 1 {
            self.console.push_str("===== pending server commands =====\n");
            self.drop_client(i, "Server command overflow");
            return;
        }
        let index = c.reliable_sequence as usize & (MAX_RELIABLE_COMMANDS - 1);
        c.reliable_commands[index] = cmd.chars().take(1023).collect();
    }

    /// `SV_SendServerCommand` (None: every client).
    pub fn send_server_command(&mut self, to: Option<usize>, cmd: &str) {
        if cmd.len() > 1022 {
            return;
        }
        match to {
            Some(i) if i < self.clients.len() => self.add_server_command(i, cmd),
            Some(_) => {}
            None => {
                if self.cvars.dedicated != 0 && cmd.starts_with("print") {
                    self.console.push_str(&format!("broadcast: {}\n", cmd.replace('\n', "\\n")));
                }
                for i in 0..self.clients.len() {
                    self.add_server_command(i, cmd);
                }
            }
        }
    }

    /// `SV_SendConfigstring` (with the bcs0/1/2 split of long strings and
    /// G_UpdateTeamConfigStrings' restrictions).
    fn send_configstring(&mut self, i: usize, index: usize) {
        let team = self.world.clients.get(i).filter(|c| c.connected).map(|c| c.team).unwrap_or(Team::None);
        if svgame::configstring_restricted(index, team) {
            self.send_server_command(Some(i), &format!("cs {index} \"\"\n"));
            return;
        }
        let s = self.configstrings[index].clone();
        let max_chunk = msg::MAX_STRING_CHARS - 24;
        if s.len() >= max_chunk {
            let b = s.as_bytes();
            let mut sent = 0usize;
            let mut remaining = b.len() as i32;
            while remaining > 0 {
                let cmd = if sent == 0 {
                    "bcs0"
                } else if (remaining as usize) < max_chunk {
                    "bcs2"
                } else {
                    "bcs1"
                };
                let end = (sent + max_chunk - 1).min(b.len());
                let part = String::from_utf8_lossy(&b[sent..end]).into_owned();
                self.send_server_command(Some(i), &format!("{cmd} {index} \"{part}\"\n"));
                sent += max_chunk - 1;
                remaining -= (max_chunk - 1) as i32;
            }
        } else {
            self.send_server_command(Some(i), &format!("cs {index} \"{s}\"\n"));
        }
    }

    /// `SV_SetConfigstring`.
    pub fn set_configstring(&mut self, index: usize, val: &str) {
        if index >= cs::MAX_CONFIGSTRINGS || self.configstrings[index] == val {
            return;
        }
        self.configstrings[index] = val.to_string();
        for i in 0..self.clients.len() {
            if self.clients[i].state < ClientState::Active {
                if self.clients[i].state == ClientState::Primed {
                    self.clients[i].cs_updated[index] = true;
                }
                continue;
            }
            self.send_configstring(i, index);
        }
    }

    /// `SV_UpdateConfigstrings`: what changed while CS_PRIMED.
    fn update_client_configstrings(&mut self, i: usize) {
        for index in 0..cs::MAX_CONFIGSTRINGS {
            if !self.clients[i].cs_updated[index] {
                continue;
            }
            self.send_configstring(i, index);
            self.clients[i].cs_updated[index] = false;
        }
    }

    /// `Cvar_InfoString( CVAR_SERVERINFO )`.
    pub fn serverinfo(&self) -> String {
        let mut s = String::new();
        let w = &self.world;
        svgame::info_set(&mut s, "sv_hostname", &self.cvars.hostname);
        svgame::info_set(&mut s, "sv_maxclients", &self.clients.len().to_string());
        svgame::info_set(&mut s, "sv_privateClients", &self.cvars.private_clients.to_string());
        svgame::info_set(&mut s, "mapname", &self.map_name);
        svgame::info_set(&mut s, "protocol", &PROTOCOL_VERSION.to_string());
        svgame::info_set(&mut s, "com_gamename", GAMENAME);
        svgame::info_set(&mut s, "version", "tremulous-legacy (Godot + Rust port)");
        svgame::info_set(&mut s, "timelimit", &w.level.timelimit.to_string());
        svgame::info_set(&mut s, "g_suddenDeathTime", &w.level.sudden_death_time.to_string());
        svgame::info_set(&mut s, "g_markDeconstruct", &w.level.mark_deconstruct.to_string());
        svgame::info_set(&mut s, "g_unlagged", "1");
        svgame::info_set(&mut s, "g_friendlyFire", &(w.level.friendly_fire as i32).to_string());
        svgame::info_set(&mut s, "sv_allowDownload", &self.cvars.allow_download.to_string());
        svgame::info_set(&mut s, "sv_dlURL", &self.cvars.dl_url);
        svgame::info_set(&mut s, "sv_minPing", &self.cvars.min_ping.to_string());
        svgame::info_set(&mut s, "sv_maxPing", &self.cvars.max_ping.to_string());
        svgame::info_set(&mut s, "g_alienStage", &w.level.alien_stage.to_string());
        svgame::info_set(&mut s, "g_humanStage", &w.level.human_stage.to_string());
        for (k_, v) in &self.cvars.serverinfo_extra {
            svgame::info_set(&mut s, k_, v);
        }
        s
    }

    /// `Cvar_InfoString_Big( CVAR_SYSTEMINFO )`.
    pub fn systeminfo(&self) -> String {
        let mut s = String::new();
        svgame::info_set(&mut s, "sv_serverid", &self.server_id.to_string());
        svgame::info_set(&mut s, "sv_cheats", if self.cvars.cheats || self.world.level.cheats { "1" } else { "0" });
        svgame::info_set(&mut s, "sv_pure", if self.pure_active() { "1" } else { "0" });
        // the server sends these to the clients so they will only load
        // pk3s also loaded at the server (sv_pure), and so they can figure
        // out which pk3s should be auto-downloaded
        if let Some(fs) = self.fs.as_ref() {
            if self.pure_active() {
                svgame::info_set(&mut s, "sv_paks", fs.loaded_pak_checksums().trim_end());
                svgame::info_set(&mut s, "sv_pakNames", &fs.loaded_pak_names());
            }
            svgame::info_set(&mut s, "sv_referencedPaks", fs.referenced_pak_checksums().trim_end());
            svgame::info_set(&mut s, "sv_referencedPakNames", &fs.referenced_pak_names());
        }
        svgame::info_set(&mut s, "g_synchronousClients", "0");
        svgame::info_set(&mut s, "pmove_fixed", "0");
        svgame::info_set(&mut s, "pmove_msec", "8");
        svgame::info_set(&mut s, "sv_fps", &self.cvars.fps.to_string());
        // wt-b VoIP: sv_voipProtocol (CVAR_SYSTEMINFO; omitted when empty)
        let vp = super::voip::voip_protocol(self.cvars.voip);
        if !vp.is_empty() {
            svgame::info_set(&mut s, "sv_voipProtocol", vp);
        }
        s
    }

    /// The game's configstrings (and CS_SERVERINFO / CS_SYSTEMINFO) into
    /// `SV_SetConfigstring`.
    fn update_configstrings(&mut self) {
        let list = svgame::configstrings(&self.world, &self.serverinfo(), &self.systeminfo(), &self.cvars.motd, &self.cvars.message);
        for (i, v) in list {
            self.set_configstring(i, &v);
        }
    }

    /// `SV_CreateBaseline`.
    fn create_baselines(&mut self) {
        for n in 1..self.world.num_entities.min(MAX_GENTITIES as usize) {
            if let Some((s, flags)) = svgame::entity_state(&self.world, n) {
                if flags & svgame::SVF_NOCLIENT == 0 {
                    self.baselines[n] = Some(s);
                }
            }
        }
    }

    // ============================================================ the frame

    /// `SV_Frame` with the packet events of this frame (`Com_EventLoop`'s
    /// NS_SERVER packets first). `msec`: real time since the last call.
    pub fn frame(&mut self, net: &mut Net, msec: i32) {
        // packets
        while let Some((from, data)) = net.get_packet(NetSrc::Server) {
            let mut m = Msg::from_bytes(&data);
            self.packet_event(net, &from, &mut m);
        }
        self.receive_masters();
        let frame_msec = self.frame_msec();
        self.time_residual += msec;
        // SV_CalcPings
        self.calc_pings();
        while self.time_residual >= frame_msec {
            self.time_residual -= frame_msec;
            self.time += frame_msec;
            self.world.real_time = real_time();
            self.world.frame(frame_msec);
            self.after_game_frame();
        }
        self.check_timeouts();
        self.send_client_messages(net);
        self.send_queued_packets(net);
        self.master_heartbeat(net, GAMENAME);
    }

    /// The engine's side of G_RunFrame's output: temp entities from events
    /// and zaps, drops, server commands, configstrings, team info.
    fn after_game_frame(&mut self) {
        let now = self.world.level.time;
        // drop expired temp entities; keep their numbers unused a while
        let expired: Vec<i32> = self.temps.iter().filter(|(_, t)| t.expire <= now).map(|(n, _)| *n).collect();
        for n in expired {
            self.temps.remove(&n);
            self.temp_free_at.insert(n, now + 1000);
        }
        // a World entity spawned into a temp slot wins it
        let clash: Vec<i32> = self.temps.keys().copied().filter(|&n| self.world.ents[n as usize].inuse).collect();
        for n in clash {
            self.temps.remove(&n);
        }
        for ev in self.world.drain_events() {
            let Some(n) = self.alloc_temp() else { break };
            let broadcast = svgame::event_is_broadcast(&ev) || (ev.entity >= 0 && self.world.ents[ev.entity as usize].svflags & svgame::SVF_BROADCAST != 0);
            let not_client = if ev.predictable && ev.entity >= 0 && (ev.entity as usize) < MAX_CLIENTS { Some(ev.entity as usize) } else { None };
            self.temps.insert(n, TempEnt { state: svgame::event_state(&ev, n), expire: now + k::EVENT_VALID_MSEC, broadcast, not_client });
        }
        // zap chains: one entity per zap while it lives
        let zaps: Vec<super::world::Zap> = self.world.zaps.clone();
        let mut live = Vec::new();
        for z in &zaps {
            let key = (z.creator, z.targets[0]);
            let n = match self.zap_nums.get(&key).copied().filter(|n| !self.world.ents[*n as usize].inuse) {
                Some(n) => n,
                None => match self.alloc_temp() {
                    Some(n) => n,
                    None => continue,
                },
            };
            self.zap_nums.insert(key, n);
            live.push(key);
            self.temps.insert(n, TempEnt { state: svgame::zap_state(&self.world, z, n), expire: now + 2 * self.frame_msec(), broadcast: false, not_client: None });
        }
        self.zap_nums.retain(|k_, _| live.contains(k_));
        // what the game said before it dropped someone reaches them first
        self.flush_game_output();
        // trap_DropClient from the game
        for (i, reason) in std::mem::take(&mut self.world.drops) {
            if i < self.clients.len() && self.clients[i].state >= ClientState::Connected {
                self.drop_client(i, &reason);
            }
        }
        // clients the game disconnected (kick, ban) lose their connection
        for i in 0..self.clients.len() {
            if self.clients[i].state >= ClientState::Connected && !self.world.clients[i].connected {
                self.drop_client(i, "was kicked");
            }
        }
        self.flush_game_output();
        self.update_configstrings();
        // CheckTeamStatus: tinfo every TEAM_LOCATION_UPDATE_TIME
        if now - self.last_team_status >= k::TEAM_LOCATION_UPDATE_TIME {
            self.last_team_status = now;
            for i in 0..self.clients.len() {
                if self.clients[i].state == ClientState::Active && self.world.clients[i].connected && self.world.clients[i].team_info != 0 {
                    if let Some(m) = svgame::teamplay_info_message(&self.world, i) {
                        self.send_server_command(Some(i), &m);
                    }
                }
            }
        }
    }

    /// A slot for a temp entity: from the top, one the World never used.
    fn alloc_temp(&mut self) -> Option<i32> {
        let now = self.world.level.time;
        let lo = self.world.num_entities.max(MAX_CLIENTS) as i32;
        let mut n = MAX_GENTITIES - 3;
        while n >= lo {
            if !self.temps.contains_key(&n) && self.temp_free_at.get(&n).is_none_or(|&t| t <= now) {
                self.temp_free_at.remove(&n);
                return Some(n);
            }
            n -= 1;
        }
        None
    }

    /// `trap_SendServerCommand` for the game's queued output.
    fn flush_game_output(&mut self) {
        for (to, cmd) in svgame::drain_server_commands(&mut self.world, &mut self.sent) {
            self.send_server_command(to, &cmd);
        }
        let con = std::mem::take(&mut self.world.console);
        self.console.push_str(&con);
    }

    /// `SV_CalcPings`.
    fn calc_pings(&mut self) {
        for i in 0..self.clients.len() {
            let c = &mut self.clients[i];
            if c.state != ClientState::Active {
                c.ping = 999;
                continue;
            }
            let (mut total, mut count) = (0, 0);
            for f in &c.frames {
                if f.message_acked <= 0 {
                    continue;
                }
                total += f.message_acked - f.message_sent;
                count += 1;
            }
            c.ping = if count == 0 { 999 } else { (total / count).min(999) };
            if self.world.clients[i].connected {
                self.world.clients[i].ps.ping = c.ping;
            }
        }
    }

    /// `SV_CheckTimeouts`.
    fn check_timeouts(&mut self) {
        let droppoint = self.time - 1000 * self.cvars.timeout;
        let zombiepoint = self.time - 1000 * self.cvars.zombietime;
        for i in 0..self.clients.len() {
            if self.clients[i].last_packet_time > self.time {
                self.clients[i].last_packet_time = self.time;
            }
            if self.clients[i].state == ClientState::Zombie && self.clients[i].last_packet_time < zombiepoint {
                // can now be reused
                self.clients[i].state = ClientState::Free;
                continue;
            }
            if self.clients[i].state >= ClientState::Connected && self.clients[i].last_packet_time < droppoint {
                self.clients[i].timeout_count += 1;
                if self.clients[i].timeout_count > 5 {
                    self.drop_client(i, "timed out");
                    // don't bother with zombie state
                    self.clients[i].state = ClientState::Free;
                }
            } else {
                self.clients[i].timeout_count = 0;
            }
        }
    }

    // ============================================================ packets

    /// `SV_PacketEvent`.
    pub fn packet_event(&mut self, net: &mut Net, from: &NetAdr, m: &mut Msg) {
        if m.cursize >= 4 && m.data[..4] == [0xff; 4] {
            self.connectionless_packet(net, from, m);
            return;
        }
        m.begin_reading_oob();
        m.read_long(); // sequence number
        let qport = m.read_short() & 0xffff;
        for i in 0..self.clients.len() {
            let c = &mut self.clients[i];
            if c.state == ClientState::Free {
                continue;
            }
            if !netchan::compare_base_adr(from, &c.netchan.remote_address) {
                continue;
            }
            // it is possible to have multiple clients from a single IP
            // address, so they are differentiated by the qport variable
            if c.netchan.qport != qport {
                continue;
            }
            // the IP port can't be used to differentiate them, because
            // some address translating routers periodically change UDP
            // port assignments
            if c.netchan.remote_address.port != from.port {
                c.netchan.remote_address.port = from.port;
            }
            // make sure it is a valid, in sequence packet
            if c.netchan.process(m) {
                // zombie clients still need to do the Netchan_Process
                // to make sure they don't need to retransmit the final
                // reliable message, but they don't do any other processing
                if c.state != ClientState::Zombie {
                    c.last_packet_time = self.time; // don't timeout
                    self.execute_client_message(net, i, m);
                }
            }
            return;
        }
    }

    /// `SVC_RateLimit` over one bucket.
    fn rate_limit(b: &mut Bucket, burst: i32, period: i32) -> bool {
        let now = now_ms();
        let interval = now - b.last_time;
        let expired = interval / period;
        let rem = interval % period;
        if expired > b.burst || interval < 0 {
            b.burst = 0;
            b.last_time = now;
        } else {
            b.burst -= expired;
            b.last_time = now - rem;
        }
        if b.burst < burst {
            b.burst += 1;
            return false;
        }
        true
    }

    /// `SVC_RateLimitAddress`.
    fn rate_limit_address(&mut self, from: &NetAdr, burst: i32, period: i32) -> bool {
        if from.kind == NetAdrType::Loopback {
            return false;
        }
        let key = if from.kind == NetAdrType::Ip6 {
            from.ip6
        } else {
            let mut k_ = [0u8; 16];
            k_[..4].copy_from_slice(&from.ip);
            k_
        };
        if self.buckets.len() >= MAX_BUCKETS && !self.buckets.contains_key(&key) {
            // the C's table is fixed-size; a map keyed by (spoofable, and
            // for ipv6 practically unlimited) source addresses must not
            // grow without bound. A bucket idle longer than burst periods
            // is drained (rate_limit would reset it), so dropping those
            // changes nothing; still full means a flood: refuse new senders.
            let now = now_ms();
            self.buckets.retain(|_, b| now.wrapping_sub(b.last_time) <= BUCKET_IDLE_MSEC);
            if self.buckets.len() >= MAX_BUCKETS {
                return true;
            }
        }
        let b = self.buckets.entry(key).or_default();
        Self::rate_limit(b, burst, period)
    }

    /// `SV_ConnectionlessPacket`.
    fn connectionless_packet(&mut self, net: &mut Net, from: &NetAdr, m: &mut Msg) {
        m.begin_reading_oob();
        m.read_long(); // skip the -1 marker
        if m.cursize >= 11 && &m.data[4..11] == b"connect" {
            let mut d = m.bytes().to_vec();
            super::huffman::decompress(&mut d, 12, MAX_MSGLEN);
            *m = Msg::from_bytes(&d);
            m.begin_reading_oob();
            m.read_long();
        }
        let s = m.read_string_line();
        let argv = super::svcmds::tokenize(&s);
        let c = argv.first().map(|x| x.to_lowercase()).unwrap_or_default();
        match c.as_str() {
            "getstatus" => self.svc_status(net, from, &argv),
            "getinfo" => self.svc_info(net, from, &argv),
            "getchallenge" => self.get_challenge(net, from, &argv),
            "connect" => self.direct_connect(net, from, &argv),
            "rcon" => self.remote_command(net, from, &s),
            "disconnect" => {
                // if a client starts up a local server, we may see some spurious
                // server disconnect messages when their new server sees our final
                // sequenced messages to the old client
            }
            _ => {}
        }
    }

    /// `SVC_Status`.
    fn svc_status(&mut self, net: &mut Net, from: &NetAdr, argv: &[String]) {
        if self.rate_limit_address(from, 10, 1000) || Self::rate_limit(&mut self.outbound_bucket, 10, 100) {
            return;
        }
        let arg1 = argv.get(1).cloned().unwrap_or_default();
        if arg1.len() > 128 {
            return;
        }
        let mut info = self.serverinfo();
        svgame::info_set(&mut info, "challenge", &arg1);
        let mut status = String::new();
        for i in 0..MAX_CLIENTS {
            // the bots are clients of this server too (ping 0)
            let net_client = i < self.clients.len() && self.clients[i].state >= ClientState::Connected;
            if net_client || (self.world.clients[i].connected && self.world.clients[i].is_bot) {
                let ps = &self.world.clients[i].ps;
                let (ping, name) = if net_client { (self.clients[i].ping, self.clients[i].name.clone()) } else { (0, self.world.clients[i].name.clone()) };
                let player = format!("{} {} \"{}\"\n", ps.persistant[crate::gen::enums::Pers::Score as usize], ping, name);
                if status.len() + player.len() >= MAX_MSGLEN {
                    break;
                }
                status.push_str(&player);
            }
        }
        net.out_of_band_print(NetSrc::Server, from, &format!("statusResponse\n{info}\n{status}"));
    }

    /// `SVC_Info`.
    fn svc_info(&mut self, net: &mut Net, from: &NetAdr, argv: &[String]) {
        if self.rate_limit_address(from, 10, 1000) || Self::rate_limit(&mut self.outbound_bucket, 10, 100) {
            return;
        }
        let arg1 = argv.get(1).cloned().unwrap_or_default();
        // Check whether Cmd_Argv(1) has a sane length (the Infostring bug)
        if arg1.len() > 128 {
            return;
        }
        let count = (self.cvars.private_clients.max(0) as usize..self.clients.len()).filter(|&i| self.clients[i].state >= ClientState::Connected).count()
            // the bots are players on this server too
            + (0..MAX_CLIENTS).filter(|&i| self.world.clients[i].connected && self.world.clients[i].is_bot).count();
        let mut info = String::new();
        svgame::info_set(&mut info, "challenge", &arg1);
        svgame::info_set(&mut info, "protocol", &PROTOCOL_VERSION.to_string());
        svgame::info_set(&mut info, "gamename", GAMENAME);
        svgame::info_set(&mut info, "hostname", &self.cvars.hostname);
        svgame::info_set(&mut info, "mapname", &self.map_name);
        svgame::info_set(&mut info, "clients", &count.to_string());
        svgame::info_set(&mut info, "sv_maxclients", &(self.clients.len() as i32 - self.cvars.private_clients).to_string());
        svgame::info_set(&mut info, "pure", "0");
        // wt-b VoIP
        let vp = super::voip::voip_protocol(self.cvars.voip);
        if !vp.is_empty() {
            svgame::info_set(&mut info, "voip", vp);
        }
        if self.cvars.min_ping != 0 {
            svgame::info_set(&mut info, "minPing", &self.cvars.min_ping.to_string());
        }
        if self.cvars.max_ping != 0 {
            svgame::info_set(&mut info, "maxPing", &self.cvars.max_ping.to_string());
        }
        net.out_of_band_print(NetSrc::Server, from, &format!("infoResponse\n{info}"));
    }

    /// `SV_GetChallenge`.
    fn get_challenge(&mut self, net: &mut Net, from: &NetAdr, argv: &[String]) {
        if self.rate_limit_address(from, 10, 1000) || Self::rate_limit(&mut self.outbound_bucket, 10, 100) {
            return;
        }
        let game_name = argv.get(2).cloned().unwrap_or_default();
        if game_name != GAMENAME {
            net.out_of_band_print(NetSrc::Server, from, &format!("print\nGame mismatch: This is a {GAMENAME} server\n"));
            return;
        }
        let client_challenge = super::spawn::c_atoi(argv.get(1).map(|s| s.as_str()).unwrap_or(""));
        // the C's loop over svs.challenges always ends with i == MAX_CHALLENGES
        // (it breaks with i = MAX_CHALLENGES), so the oldest slot is taken
        let mut oldest = 0usize;
        let mut oldest_time = i32::MAX;
        for (i, ch) in self.challenges.iter().enumerate() {
            if ch.time < oldest_time {
                oldest_time = ch.time;
                oldest = i;
            }
        }
        let idx = oldest;
        {
            // this is the first time this client has asked for a challenge
            let ch = &mut self.challenges[idx];
            ch.client_challenge = client_challenge;
            ch.adr = *from;
            ch.first_time = self.time;
            ch.connected = false;
        }
        let r = (self.rand() << 16) ^ self.rand() ^ self.time;
        let ch = &mut self.challenges[idx];
        ch.challenge = r;
        ch.was_refused = false;
        ch.time = self.time;
        ch.ping_time = self.time;
        let (c, cc) = (ch.challenge, client_challenge);
        net.out_of_band_print(NetSrc::Server, from, &format!("challengeResponse {c} {cc} {PROTOCOL_VERSION}"));
    }

    /// `SV_DirectConnect`.
    fn direct_connect(&mut self, net: &mut Net, from: &NetAdr, argv: &[String]) {
        let mut userinfo: String = argv.get(1).cloned().unwrap_or_default().chars().take(msg::MAX_STRING_CHARS - 1).collect();
        let version = super::spawn::c_atoi(&svgame::info_value(&userinfo, "protocol"));
        if version != PROTOCOL_VERSION {
            net.out_of_band_print(NetSrc::Server, from, &format!("print\nServer uses protocol version {PROTOCOL_VERSION} (yours is {version}).\n"));
            return;
        }
        let challenge = super::spawn::c_atoi(&svgame::info_value(&userinfo, "challenge"));
        let qport = super::spawn::c_atoi(&svgame::info_value(&userinfo, "qport"));
        // quick reject
        for c in &self.clients {
            if c.state == ClientState::Free {
                continue;
            }
            if netchan::compare_base_adr(from, &c.netchan.remote_address) && (c.netchan.qport == qport || from.port == c.netchan.remote_address.port) {
                if self.time - c.last_connect_time < self.cvars.reconnect_limit * 1000 {
                    return; // reconnect rejected: too soon
                }
                break;
            }
        }
        let ip = if netchan::is_local_address(from) { "localhost".to_string() } else { from.to_string_base() };
        if ip.len() + userinfo.len() + 4 >= msg::MAX_STRING_CHARS {
            net.out_of_band_print(NetSrc::Server, from, "print\nUserinfo string length exceeded.  Try removing setu cvars from your config.\n");
            return;
        }
        svgame::info_set(&mut userinfo, "ip", &ip);
        // see if the challenge is valid (LAN clients don't need to challenge)
        if !netchan::is_local_address(from) {
            let Some(ci) = (0..MAX_CHALLENGES).find(|&i| netchan::compare_adr(from, &self.challenges[i].adr) && self.challenges[i].challenge == challenge) else {
                net.out_of_band_print(NetSrc::Server, from, "print\nNo or bad challenge for your address.\n");
                return;
            };
            if self.challenges[ci].was_refused {
                // Return silently, so that error messages written by the server keep being displayed.
                return;
            }
            let ping = self.time - self.challenges[ci].ping_time;
            // never reject a LAN client based on ping
            if !netchan::is_lan_address(from) {
                if self.cvars.min_ping != 0 && ping < self.cvars.min_ping {
                    net.out_of_band_print(NetSrc::Server, from, "print\nServer is for high pings only\n");
                    self.challenges[ci].was_refused = true;
                    return;
                }
                if self.cvars.max_ping != 0 && ping > self.cvars.max_ping {
                    net.out_of_band_print(NetSrc::Server, from, "print\nServer is for low pings only\n");
                    self.challenges[ci].was_refused = true;
                    return;
                }
            }
            self.console.push_str(&format!("Client {ci} connecting with {ping} challenge ping\n"));
            self.challenges[ci].connected = true;
        }
        // if there is already a slot for this ip, reuse it
        let mut slot = None;
        for i in 0..self.clients.len() {
            let c = &self.clients[i];
            if c.state == ClientState::Free {
                continue;
            }
            if netchan::compare_base_adr(from, &c.netchan.remote_address) && (c.netchan.qport == qport || from.port == c.netchan.remote_address.port) {
                self.console.push_str(&format!("{}:reconnect\n", from.to_string_port()));
                // the game forgets the old connection
                if self.world.clients[i].connected {
                    self.world.disconnect(i);
                }
                slot = Some(i);
                break;
            }
        }
        if slot.is_none() {
            // find a client slot: the private ones need sv_privatePassword
            let password = svgame::info_value(&userinfo, "password");
            let start = if !self.cvars.private_password.is_empty() && password == self.cvars.private_password { 0 } else { self.cvars.private_clients.max(0) as usize };
            slot = (start..self.clients.len()).find(|&i| self.clients[i].state == ClientState::Free && !self.world.clients[i].connected);
            if slot.is_none() {
                net.out_of_band_print(NetSrc::Server, from, "print\nServer is full\n");
                return;
            }
        }
        let i = slot.unwrap();
        let mut nc = SvClient::new(Netchan::setup(NetSrc::Server, *from, qport, challenge));
        nc.challenge = challenge;
        nc.userinfo = userinfo;
        self.clients[i] = nc;
        // ClientConnect
        let info = self.connect_info(i);
        if let Err(reason) = self.world.connect_ex(Some(i), &info, false) {
            net.out_of_band_print(NetSrc::Server, from, &format!("print\n{reason}\n"));
            self.clients[i] = SvClient::new(Netchan::setup(NetSrc::Server, NetAdr::default(), 0, 0));
            self.flush_game_output();
            return;
        }
        self.userinfo_changed(i);
        // send the connect packet to the client
        net.out_of_band_print(NetSrc::Server, from, &format!("connectResponse {challenge}"));
        let c = &mut self.clients[i];
        c.state = ClientState::Connected;
        c.last_snapshot_time = 0;
        c.last_packet_time = self.time;
        c.last_connect_time = self.time;
        // when we receive the first packet from the client, we will
        // notice that it is from a different serverid and that the
        // gamestate message was not just sent, forcing a retransmit
        c.gamestate_message_num = -1;
        self.flush_game_output();
        // if this was the first client on the server, or the last client
        // the server can hold, send a heartbeat to the master
        let count = self.clients.iter().filter(|c| c.state >= ClientState::Connected).count();
        if count == 1 || count == self.clients.len() {
            self.next_heartbeat_time = -9999;
        }
    }

    /// `SVC_RemoteCommand`: the line is queued for the host's console when
    /// the password is right; the host answers with `rcon_reply`.
    fn remote_command(&mut self, net: &mut Net, from: &NetAdr, line: &str) {
        if self.rate_limit_address(from, 10, 1000) {
            return;
        }
        let argv = super::svcmds::tokenize(line);
        let pass = argv.get(1).cloned().unwrap_or_default();
        let valid = !self.cvars.rcon_password.is_empty() && pass == self.cvars.rcon_password;
        // the command: what follows the password
        let rest = {
            let s = line.trim_start();
            let s = s.get(4..).unwrap_or("").trim_start();
            let s = s.trim_start_matches(|c: char| c != ' ').trim_start();
            s.to_string()
        };
        if !valid {
            if Self::rate_limit(&mut self.rcon_bucket, 10, 1000) {
                return;
            }
            self.console.push_str(&format!("Bad rcon from {}: {rest}\n", from.to_string_base()));
            let msg = if self.cvars.rcon_password.is_empty() { "No rconpassword set on the server.\n" } else { "Bad rconpassword.\n" };
            net.out_of_band_print(NetSrc::Server, from, &format!("print\n{msg}"));
            return;
        }
        self.console.push_str(&format!("Rcon from {}: {rest}\n", from.to_string_base()));
        self.rcon_queue.push((*from, rest));
    }

    /// `SV_FlushRedirect`: the output of an rcon command.
    pub fn rcon_reply(&mut self, net: &mut Net, to: &NetAdr, text: &str) {
        // SV_OUTPUTBUF_LENGTH chunks
        let b = text.as_bytes();
        for chunk in b.chunks(1024 - 16) {
            net.out_of_band_print(NetSrc::Server, to, &format!("print\n{}", String::from_utf8_lossy(chunk)));
        }
        if b.is_empty() {
            net.out_of_band_print(NetSrc::Server, to, "print\n");
        }
    }

    // ============================================================ clients

    /// `SV_UserinfoChanged`.
    fn userinfo_changed(&mut self, i: usize) {
        let lan = netchan::is_lan_address(&self.clients[i].netchan.remote_address);
        let fps = self.cvars.fps;
        let (dedicated, lan_force) = (self.cvars.dedicated, self.cvars.lan_force_rate);
        let c = &mut self.clients[i];
        c.name = svgame::info_value(&c.userinfo, "name");
        // rate command
        if lan && dedicated != 2 && lan_force {
            c.rate = 99999; // lans should not rate limit
        } else {
            let v = svgame::info_value(&c.userinfo, "rate");
            c.rate = if v.is_empty() { 3000 } else { super::spawn::c_atoi(&v).clamp(1000, 90000) };
        }
        // snaps command
        let v = svgame::info_value(&c.userinfo, "snaps");
        let ms = if v.is_empty() { 50 } else { 1000 / super::spawn::c_atoi(&v).clamp(1, fps) };
        if ms != c.snapshot_msec {
            c.last_snapshot_time = 0;
            c.snapshot_msec = ms;
        }
        let ui = c.userinfo.clone();
        c.voip.userinfo_changed(&ui); // wt-b VoIP: cl->hasVoip
        // TTimo: maintain the IP information
        let ip = if netchan::is_local_address(&c.netchan.remote_address) { "localhost".to_string() } else { c.netchan.remote_address.to_string_base() };
        svgame::info_set(&mut c.userinfo, "ip", &ip);
    }

    /// `SV_DropClient`.
    pub fn drop_client(&mut self, i: usize, reason: &str) {
        if self.clients[i].state == ClientState::Zombie || self.clients[i].state == ClientState::Free {
            return; // already dropped
        }
        // see if we already have a challenge for this ip
        let adr = self.clients[i].netchan.remote_address;
        if let Some(ch) = self.challenges.iter_mut().find(|ch| netchan::compare_adr(&adr, &ch.adr)) {
            *ch = Challenge::default();
        }
        self.clients[i].netchan_queue.clear();
        self.clients[i].download = Download::default();
        self.clients[i].voip.free(); // wt-b VoIP: SV_FreeClient
        // tell everyone why they got dropped
        let name = self.clients[i].name.clone();
        self.send_server_command(None, &format!("print \"{name}^7 {reason}\n\""));
        // call the prog function for removing a client
        if self.world.clients[i].connected {
            self.world.disconnect(i);
        }
        // add the disconnect command
        self.send_server_command(Some(i), &format!("disconnect \"{reason}\""));
        self.clients[i].userinfo.clear();
        self.clients[i].state = ClientState::Zombie; // become free in a few seconds
        self.flush_game_output();
        if self.clients.iter().all(|c| c.state < ClientState::Connected) {
            self.next_heartbeat_time = -9999;
        }
    }

    /// `SV_SendClientGameState`.
    fn send_client_gamestate(&mut self, net: &mut Net, i: usize) {
        let c = &mut self.clients[i];
        c.state = ClientState::Primed;
        c.pure_authentic = false;
        c.got_cp = false;
        c.cs_updated.iter_mut().for_each(|x| *x = false);
        // when we receive the first packet from the client, we will
        // notice that it is from a different serverid and that the
        // gamestate message was not just sent, forcing a retransmit
        c.gamestate_message_num = c.netchan.outgoing_sequence;
        let mut m = Msg::new(MAX_MSGLEN);
        // NOTE, MRE: all server->client messages now acknowledge
        m.write_long(c.last_client_command);
        // send any server commands waiting to be sent first.
        Self::update_server_commands(c, &mut m);
        // send the gamestate
        m.write_byte(svc::GAMESTATE);
        m.write_long(c.reliable_sequence);
        // write the configstrings
        for (n, s) in self.configstrings.iter().enumerate() {
            if !s.is_empty() {
                m.write_byte(svc::CONFIGSTRING);
                m.write_short(n as i32);
                m.write_big_string(s);
            }
        }
        // write the baselines
        let null = EntityState::default();
        for b in self.baselines.iter().flatten() {
            m.write_byte(svc::BASELINE);
            m.write_delta_entity(Some(&null), Some(b), true);
        }
        m.write_byte(svc::EOF);
        m.write_long(i as i32);
        // write the checksum feed
        m.write_long(self.checksum_feed);
        if m.overflowed {
            self.console.push_str(&format!("WARNING: gamestate overflowed for {}\n", self.clients[i].name));
        }
        self.send_message_to_client(net, i, m);
    }

    /// `SV_ClientEnterWorld` (ClientBegin ran at connect time, dec033).
    fn client_enter_world(&mut self, i: usize, cmd: Option<&UserCmd>) {
        self.clients[i].state = ClientState::Active;
        // resend all configstrings using the cs commands since these are
        // no longer sent when the client is CS_PRIMED
        self.update_client_configstrings(i);
        let c = &mut self.clients[i];
        c.delta_message = -1;
        c.last_snapshot_time = 0; // generate a snapshot immediately
        c.last_usercmd = cmd.copied().unwrap_or_default();
    }

    /// `SV_UpdateServerCommandsToClient`.
    fn update_server_commands(c: &mut SvClient, m: &mut Msg) {
        // write any unacknowledged serverCommands
        for i in c.reliable_acknowledge + 1..=c.reliable_sequence {
            m.write_byte(svc::SERVER_COMMAND);
            m.write_long(i);
            m.write_string(&c.reliable_commands[i as usize & (MAX_RELIABLE_COMMANDS - 1)]);
        }
        c.reliable_sent = c.reliable_sequence;
    }

    /// `SV_ExecuteClientMessage`.
    fn execute_client_message(&mut self, net: &mut Net, i: usize, m: &mut Msg) {
        m.bitstream();
        let server_id = m.read_long();
        let c = &mut self.clients[i];
        c.message_acknowledge = m.read_long();
        if c.message_acknowledge < 0 {
            // usually only hackers create messages like this
            return;
        }
        // a download message this packet acknowledges for the first time:
        // a round trip sample no resend or held-back "nextdl" can blur
        let ack = c.message_acknowledge;
        let d = &mut c.download;
        if ack > d.rtt_acked {
            d.rtt_acked = ack;
            if let Some(&(seq, sent)) = d.msg_sent.get(ack as usize % DOWNLOAD_MSG_RING) {
                if seq == ack {
                    let sample = (self.time - sent).max(1);
                    d.srtt = if d.srtt == 0 { sample } else { (7 * d.srtt + sample) / 8 };
                }
            }
        }
        c.reliable_acknowledge = m.read_long();
        // NOTE: when the client message is fux0red the acknowledgement numbers
        // can be out of range, this could cause the server to send thousands of
        // server commands which the server thinks are not yet acknowledged
        if c.reliable_acknowledge < c.reliable_sequence - MAX_RELIABLE_COMMANDS as i32 {
            c.reliable_acknowledge = c.reliable_sequence;
            return;
        }
        // if this is a usercmd from a previous gamestate, ignore it or
        // retransmit the current gamestate
        if server_id != self.server_id && c.download.name.is_empty() && !c.last_client_command_string.contains("nextdl") {
            if server_id >= self.restarted_server_id && server_id < self.server_id {
                // they just haven't caught the map_restart yet
                return;
            }
            // if we can tell that the client has dropped the last
            // gamestate we sent them, resend it
            if c.message_acknowledge > c.gamestate_message_num {
                self.send_client_gamestate(net, i);
            }
            return;
        }
        // this client has acknowledged the new gamestate so it's
        // safe to start sending it the real time again
        if c.old_server_time != 0 && server_id == self.server_id {
            c.old_server_time = 0;
        }
        // read optional clientCommand strings
        let mut cmd;
        loop {
            cmd = m.read_byte();
            if cmd == clc::EOF || cmd != clc::CLIENT_COMMAND {
                break;
            }
            if !self.client_command(net, i, m) {
                return; // we couldn't execute it because of the flood protection
            }
            if self.clients[i].state == ClientState::Zombie {
                return; // disconnect command
            }
        }
        // wt-b VoIP: skip legacy speex voip data, read optional voip data
        let cmd = self.read_client_voip(i, m, cmd);
        if cmd == clc::MOVE {
            self.user_move(net, i, m, true);
        } else if cmd == clc::MOVE_NO_DELTA {
            self.user_move(net, i, m, false);
        } else if cmd != clc::EOF {
            self.console.push_str(&format!("WARNING: bad command byte for client {i}\n"));
        }
    }

    /// `SV_ClientCommand`.
    fn client_command(&mut self, net: &mut Net, i: usize, m: &mut Msg) -> bool {
        let seq = m.read_long();
        let s = m.read_string();
        // see if we have already executed it
        if self.clients[i].last_client_command >= seq {
            return true;
        }
        // drop the connection if we have somehow lost commands
        if seq > self.clients[i].last_client_command + 1 {
            let n = self.clients[i].name.clone();
            self.console.push_str(&format!("Client {n} lost {} clientCommands\n", seq - self.clients[i].last_client_command + 1));
            self.drop_client(i, "Lost reliable commands");
            return false;
        }
        self.clients[i].next_reliable_time = self.time + 1000;
        self.execute_client_command(net, i, &s, true);
        self.clients[i].last_client_command = seq;
        self.clients[i].last_client_command_string = s;
        true
    }

    /// `SV_ExecuteClientCommand`: the engine's ucmds, else the game's ClientCommand.
    fn execute_client_command(&mut self, net: &mut Net, i: usize, s: &str, client_ok: bool) {
        let argv = super::svcmds::tokenize(s);
        let a0 = argv.first().cloned().unwrap_or_default();
        match a0.as_str() {
            "userinfo" => {
                // SV_UpdateUserinfo_f
                self.clients[i].userinfo = argv.get(1).cloned().unwrap_or_default().chars().take(msg::MAX_STRING_CHARS - 1).collect();
                self.userinfo_changed(i);
                // ClientUserinfoChanged
                if self.world.clients[i].connected {
                    let ui = self.clients[i].userinfo.clone();
                    let name = svgame::info_value(&ui, "name");
                    super::client::userinfo_changed(&mut self.world, i, &name, false);
                    let c = &mut self.world.clients[i];
                    let u = svgame::info_value(&ui, "cg_unlagged");
                    c.use_unlagged = u.is_empty() || super::spawn::c_atoi(&u) != 0;
                    let t = super::spawn::c_atoi(&svgame::info_value(&ui, "teamoverlay")) != 0;
                    if !t {
                        c.team_info = 0;
                    } else if c.team_info == 0 {
                        c.team_info = 1;
                    }
                }
                self.flush_game_output();
            }
            "disconnect" => self.drop_client(i, "disconnected"),
            "cp" => self.verify_paks(net, i, &argv),
            "vdr" => {
                // SV_ResetPureClient_f
                self.clients[i].pure_authentic = false;
                self.clients[i].got_cp = false;
            }
            "download" => {
                // SV_BeginDownload_f
                self.clients[i].download = Download { name: argv.get(1).cloned().unwrap_or_default(), ..Default::default() };
            }
            "nextdl" => {
                // SV_NextDownload_f
                let block = super::spawn::c_atoi(argv.get(1).map(|x| x.as_str()).unwrap_or("")) as usize;
                let d = &mut self.clients[i].download;
                if block == d.client_block {
                    // find out if we are done.  A zero-length block indicates EOF
                    if d.blocks.get(d.client_block % MAX_DOWNLOAD_WINDOW).is_some_and(|b| b.is_empty()) && d.eof && d.client_block + 1 == d.current_block {
                        self.console.push_str(&format!("clientDownload: {i} : file \"{}\" completed\n", d.name));
                        *d = Download::default();
                        return;
                    }
                    d.send_time = self.time;
                    d.client_block += 1;
                    return;
                }
                // we aren't getting an acknowledge for the correct block, drop the client
                self.drop_client(i, "broken download");
            }
            "stopdl" => self.clients[i].download = Download::default(),
            // wt-b VoIP: SV_Voip_f
            "voip" => self.clients[i].voip.voip_f(&argv),
            "donedl" => {
                // SV_DoneDownload_f
                if self.clients[i].state != ClientState::Active {
                    self.send_client_gamestate(net, i);
                }
            }
            _ => {
                if client_ok && matches!(self.clients[i].state, ClientState::Active | ClientState::Primed) && self.world.clients[i].connected {
                    // GAME_CLIENT_COMMAND
                    self.world.real_time = real_time();
                    let line = args_sanitize(s);
                    if let Err(e) = super::client::command(&mut self.world, i, &line) {
                        if !e.is_empty() {
                            self.send_server_command(Some(i), &format!("print \"{}\n\"", e.replace('"', "'")));
                        }
                    }
                    self.flush_game_output();
                }
            }
        }
    }

    /// `SV_VerifyPaks_f`: the client's "cp <serverId> cgame ui @ refs...
    /// encoded" against the paks this server loaded.
    fn verify_paks(&mut self, net: &mut Net, i: usize, argv: &[String]) {
        // if we are pure, we "expect" the client to load certain things from
        // certain pk3 files, namely we want the client to have loaded the
        // ui and cgame that we think should be loaded based on the pure setting
        if !self.pure_active() {
            return;
        }
        let fs = self.fs.as_ref().unwrap();
        // we run the game, so determine which cgame and ui the client "should" be running
        let cg = fs.module_file(&files::CGAME_MODULE).and_then(|f| fs.file_is_in_pak(f));
        let ui = fs.module_file(&files::UI_MODULE).and_then(|f| fs.file_is_in_pak(f));
        let mut good = cg.is_some() && ui.is_some();
        let (n_chk_sum1, n_chk_sum2) = (cg.unwrap_or(0), ui.unwrap_or(0));
        let n_client_paks = argv.len();
        // start at arg 1 ( skip the command )
        let mut cur = 1usize;
        let arg = |k: usize| argv.get(k).map(|s| s.as_str()).unwrap_or("");
        match argv.get(cur) {
            None => good = false,
            Some(a) => {
                // we may get incoming cp sequences from a previous checksumFeed, which we need to ignore
                // since serverId is a frame count, it always goes up
                if super::spawn::c_atoi(a) < self.checksum_feed_server_id {
                    return;
                }
            }
        }
        cur += 1;
        // we basically use this while loop to avoid using 'goto' :)
        while good {
            // must be at least 6: "cl_paks cgame ui @ firstref ... numChecksums"
            // numChecksums is encoded
            if n_client_paks < 6 {
                good = false;
                break;
            }
            // verify first to be the cgame checksum
            let a = arg(cur);
            cur += 1;
            if a.starts_with('@') || super::spawn::c_atoi(a) != n_chk_sum1 {
                good = false;
                break;
            }
            // verify the second to be the ui checksum
            let a = arg(cur);
            cur += 1;
            if a.starts_with('@') || super::spawn::c_atoi(a) != n_chk_sum2 {
                good = false;
                break;
            }
            // should be sitting at the delimeter now
            if !arg(cur).starts_with('@') {
                good = false;
                break;
            }
            cur += 1;
            // store checksums since tokenization is not re-entrant
            let client: Vec<i32> = (cur..n_client_paks).map(|k| super::spawn::c_atoi(arg(k))).collect();
            // store number to compare against (minus one cause the last is the number of checksums)
            let n = client.len().saturating_sub(1);
            // make sure none of the client check sums are the same
            // so the client can't send 5 the same checksums
            'dup: for a in 0..n {
                for b in 0..n {
                    if a != b && client[a] == client[b] {
                        good = false;
                        break 'dup;
                    }
                }
            }
            if !good {
                break;
            }
            // get the pure checksums of the pk3 files loaded by the server
            let server: Vec<i32> = fs.loaded_pak_pure_checksums().split_whitespace().take(1024).map(super::spawn::c_atoi).collect();
            // check if the client has provided any pure checksums of pk3 files not loaded by the server
            if client[..n].iter().any(|c| !server.contains(c)) {
                good = false;
                break;
            }
            // check if the number of checksums was correct
            let mut x = self.checksum_feed;
            for c in &client[..n] {
                x ^= c;
            }
            x ^= n as i32;
            if client.get(n) != Some(&x) {
                good = false;
            }
            // break out
            break;
        }
        self.clients[i].got_cp = true;
        if good {
            self.clients[i].pure_authentic = true;
        } else {
            self.clients[i].pure_authentic = false;
            self.clients[i].last_snapshot_time = 0;
            self.clients[i].state = ClientState::Active;
            self.send_client_snapshot(net, i);
            self.send_server_command(Some(i), "disconnect \"Unpure Client. You may need to enable in-game downloads to connect to this server (set cl_allowDownload 1)\"");
            self.drop_client(i, "Unpure client detected. Invalid .PK3 files referenced!");
        }
    }

    /// `SV_UserMove`.
    fn user_move(&mut self, net: &mut Net, i: usize, m: &mut Msg, delta: bool) {
        let pure = self.pure_active();
        let c = &mut self.clients[i];
        c.delta_message = if delta { c.message_acknowledge } else { -1 };
        let cmd_count = m.read_byte();
        if cmd_count < 1 || cmd_count > MAX_PACKET_USERCMDS {
            return;
        }
        // use the checksum feed in the key
        let mut key = self.checksum_feed;
        // also use the message acknowledge
        key ^= c.message_acknowledge;
        // also use the last acknowledged server command in the key
        key ^= msg::hash_key(&c.reliable_commands[c.reliable_acknowledge as usize & (MAX_RELIABLE_COMMANDS - 1)], 32);
        let mut cmds = Vec::with_capacity(cmd_count as usize);
        let mut old = UserCmd::default();
        for _ in 0..cmd_count {
            let cmd = m.read_delta_usercmd_key(key, &old);
            old = cmd;
            cmds.push(cmd);
        }
        // save time for ping calculation
        let ma = c.message_acknowledge as usize & PACKET_MASK;
        c.frames[ma].message_acked = self.time;
        // TTimo
        // catch the no-cp-yet situation before SV_ClientEnterWorld
        // if CS_ACTIVE, then it's time to trigger a new gamestate emission
        // if not, then we are getting remaining parasite usermove commands, which we should ignore
        if pure && !c.pure_authentic && !c.got_cp {
            if c.state == ClientState::Active {
                // we didn't get a cp yet, don't assume anything and just send the gamestate all over again
                self.send_client_gamestate(net, i);
            }
            return;
        }
        // if this is the first usercmd we have received this gamestate,
        // put the client into the world
        if c.state == ClientState::Primed {
            self.client_enter_world(i, cmds.first());
            // the moves can be processed normaly
        }
        // a bad cp command was sent, drop the client
        if pure && !self.clients[i].pure_authentic {
            self.drop_client(i, "Cannot validate pure client!");
            return;
        }
        if self.clients[i].state != ClientState::Active {
            self.clients[i].delta_message = -1;
            return;
        }
        // usually, the first couple commands will be duplicates
        // of ones we have previously received, but the servertimes
        // in the commands will cause them to be immediately discarded
        let last = cmds.last().map(|c| c.server_time).unwrap_or(0);
        for cmd in cmds {
            // if this is a cmd from before a map_restart ignore it
            if cmd.server_time > last {
                continue;
            }
            // don't execute if this is an old cmd which is already executed
            // these old cmds are included when cl_packetdup > 0
            if cmd.server_time <= self.clients[i].last_usercmd.server_time {
                continue;
            }
            // SV_ClientThink
            self.clients[i].last_usercmd = cmd;
            if self.clients[i].state != ClientState::Active {
                return; // may have been kicked during the last usercmd
            }
            super::client::client_think(&mut self.world, i, cmd);
        }
        self.flush_game_output();
    }

    // ============================================================ snapshots

    /// `SV_BuildClientSnapshot`.
    fn build_client_snapshot(&mut self, i: usize) {
        let seq = self.clients[i].netchan.outgoing_sequence as usize & PACKET_MASK;
        let mut frame = ClientSnapshot::default();
        if self.clients[i].state == ClientState::Zombie || !self.world.clients[i].connected {
            self.clients[i].frames[seq] = frame;
            return;
        }
        // grab the current playerState_t
        frame.ps = self.world.clients[i].ps;
        let client_num = frame.ps.client_num;
        // find the client's viewpoint
        let mut org = frame.ps.origin;
        org[2] += frame.ps.viewheight as f32;
        let mut nums: Vec<i32> = Vec::new();
        let mut seen = vec![false; MAX_GENTITIES as usize];
        // never send client's own entity, because it can
        // be regenerated from the playerstate
        if (0..MAX_GENTITIES).contains(&client_num) {
            seen[client_num as usize] = true;
        }
        self.add_entities_visible_from_point(&org, &mut frame, &mut nums, &mut seen, client_num, i, false);
        // if there were portals visible, there may be out of order entities
        // in the list which will need to be resorted for the delta compression
        // to work correctly.  This also catches the error condition
        // of an entity being included twice.
        nums.sort_unstable();
        // now that all viewpoint's areabits have been OR'd together, invert
        // all of them to make it a mask vector
        for b in frame.areabits.iter_mut() {
            *b ^= 0xff;
        }
        for n in nums {
            let s = match self.temps.get(&n) {
                Some(t) => t.state,
                None => match svgame::entity_state(&self.world, n as usize) {
                    Some((s, _)) => s,
                    None => continue,
                },
            };
            frame.entities.push(s);
        }
        self.clients[i].frames[seq] = frame;
    }

    /// `SV_AddEntitiesVisibleFromPoint` (with the temp entities).
    #[allow(clippy::too_many_arguments)]
    fn add_entities_visible_from_point(&self, origin: &Vec3, frame: &mut ClientSnapshot, nums: &mut Vec<i32>, seen: &mut [bool], client_num: i32, client: usize, portal: bool) {
        let w = &self.world;
        let cm = &w.cm;
        let leafnum = cm.point_leafnum(origin);
        let (clientarea, clientcluster) = cm.leafs.get(leafnum as usize).map(|l| (l.area, l.cluster)).unwrap_or((-1, -1));
        // calculate the visible areas
        frame.areabytes = write_area_bits(w, &mut frame.areabits, clientarea);
        let clientpvs = cm.cluster_pvs(clientcluster).to_vec();
        // a map without visibility / area data (the sheet arena) sees
        // everything, as a BSP without a vis lump does (all-ones PVS, one area)
        let no_vis = !cm.vised || clientpvs.is_empty();
        let no_areas = cm.num_areas <= 0;
        let visible = |sv: &svgame::SvEntity| -> bool {
            // ignore if not touching a PV leaf
            // check area
            if !no_areas && !cm.areas_connected(clientarea, sv.areanum) && !cm.areas_connected(clientarea, sv.areanum2) {
                // doors can legally straddle two areas, so
                // we may need to check another one
                return false; // blocked by a door
            }
            if no_vis {
                return true;
            }
            if sv.clusternums.is_empty() {
                return false;
            }
            let bit = |l: i32| l >= 0 && clientpvs.get((l >> 3) as usize).is_some_and(|b| b & (1 << (l & 7)) != 0);
            if sv.clusternums.iter().any(|&l| bit(l)) {
                return true;
            }
            // if we haven't found it to be visible,
            // check overflow clusters that coudln't be stored
            if sv.last_cluster != 0 {
                let mut l = *sv.clusternums.last().unwrap();
                while l <= sv.last_cluster {
                    if bit(l) {
                        return true;
                    }
                    l += 1;
                }
            }
            false
        };
        let mut portals = Vec::new();
        for e in 0..w.num_entities.min(MAX_GENTITIES as usize) {
            if seen[e] {
                continue;
            }
            let Some((s, flags)) = svgame::entity_state(w, e) else { continue };
            // entities can be flagged to explicitly not be sent to the client
            if flags & svgame::SVF_NOCLIENT != 0 {
                continue;
            }
            // broadcast entities are always sent
            if flags & svgame::SVF_BROADCAST != 0 {
                seen[e] = true;
                nums.push(e as i32);
                continue;
            }
            let ent = &w.ents[e];
            let sv = if ent.bmodel || ent.etype == super::world::EType::ModelDoor {
                let (lo, hi) = ent.abs_bounds();
                let mut s2 = svgame::link_box(w, &lo, &hi);
                if ent.areanum != -1 || ent.areanum2 != -1 {
                    s2.areanum = ent.areanum;
                    s2.areanum2 = ent.areanum2;
                }
                s2
            } else {
                let (lo, hi) = ent.abs_bounds();
                svgame::link_box(w, &lo, &hi)
            };
            if !visible(&sv) {
                continue;
            }
            seen[e] = true;
            if nums.len() < MAX_SNAPSHOT_ENTITIES {
                nums.push(e as i32);
            }
            // if this is a portal entity, add everything visible from its camera position
            if flags & svgame::SVF_PORTAL != 0 {
                if s.generic1 != 0 {
                    let d = super::qmath::sub(&s.origin, origin);
                    if super::qmath::dot(&d, &d) > (s.generic1 as f32) * (s.generic1 as f32) {
                        continue;
                    }
                }
                portals.push(s.origin2);
            }
        }
        // the temp entities (events, zap chains)
        for (&n, t) in self.temps.iter() {
            if seen[n as usize] || t.not_client == Some(client_num as usize) {
                continue;
            }
            let o = t.state.pos.tr_base;
            if t.broadcast || visible(&svgame::link_box(w, &[o[0] - 1.0, o[1] - 1.0, o[2] - 1.0], &[o[0] + 1.0, o[1] + 1.0, o[2] + 1.0])) {
                seen[n as usize] = true;
                if nums.len() < MAX_SNAPSHOT_ENTITIES {
                    nums.push(n);
                }
            }
        }
        let _ = portal;
        for p in portals {
            let mut f2 = ClientSnapshot::default();
            self.add_entities_visible_from_point(&p, &mut f2, nums, seen, client_num, client, true);
            for (a, b) in frame.areabits.iter_mut().zip(f2.areabits.iter()) {
                *a |= *b;
            }
        }
    }

    /// `SV_WriteSnapshotToClient`.
    fn write_snapshot_to_client(&mut self, i: usize, m: &mut Msg) {
        let c = &self.clients[i];
        let seq = c.netchan.outgoing_sequence;
        // try to use a previous frame as the source for delta compressing the snapshot
        let (oldframe, lastframe) = if c.delta_message <= 0 || c.state != ClientState::Active {
            // client is asking for a retransmit
            (None, 0)
        } else if seq - c.delta_message >= (PACKET_BACKUP as i32 - 3) {
            // client hasn't gotten a good message through in a long time
            (None, 0)
        } else {
            // we have a valid snapshot to delta from
            (Some(c.delta_message as usize & PACKET_MASK), seq - c.delta_message)
        };
        let frame = &c.frames[seq as usize & PACKET_MASK];
        m.write_byte(svc::SNAPSHOT);
        // send over the current server time so the client can drift
        // its view of time to try to match
        let st = if c.old_server_time != 0 { self.world.level.time + c.old_server_time } else { self.world.level.time };
        m.write_long(st);
        // what we are delta'ing from
        m.write_byte(lastframe);
        let mut snap_flags = self.snap_flag_server_bit;
        if c.rate_delayed {
            snap_flags |= SNAPFLAG_RATE_DELAYED;
        }
        if c.state != ClientState::Active {
            snap_flags |= SNAPFLAG_NOT_ACTIVE;
        }
        m.write_byte(snap_flags);
        // send over the areabits
        m.write_byte(frame.areabytes as i32);
        m.write_data(&frame.areabits[..frame.areabytes]);
        // delta encode the playerstate
        let old = oldframe.map(|o| &c.frames[o]);
        m.write_delta_playerstate(old.map(|o| &o.ps), &frame.ps);
        // delta encode the entities
        emit_packet_entities(old, frame, &self.baselines, m);
        // padding for rate debugging
        for _ in 0..self.cvars.pad_packets {
            m.write_byte(svc::NOP);
        }
    }

    /// `SV_SendMessageToClient`.
    fn send_message_to_client(&mut self, net: &mut Net, i: usize, m: Msg) {
        let t = self.time;
        let c = &mut self.clients[i];
        let f = &mut c.frames[c.netchan.outgoing_sequence as usize & PACKET_MASK];
        // record information about the message
        f.message_size = m.cursize;
        f.message_sent = t;
        f.message_acked = -1;
        // send the datagram
        self.netchan_transmit(net, i, m);
    }

    /// `SV_Netchan_Transmit`.
    fn netchan_transmit(&mut self, net: &mut Net, i: usize, mut m: Msg) {
        m.write_byte(svc::EOF);
        let c = &mut self.clients[i];
        if c.netchan.unsent_fragments || !c.netchan_queue.is_empty() {
            c.netchan_queue.push_back(m.bytes().to_vec());
        } else {
            c.netchan.transmit(net, m.bytes(), now_ms());
        }
    }

    /// `SV_SendClientSnapshot`.
    pub fn send_client_snapshot(&mut self, net: &mut Net, i: usize) {
        // build the snapshot
        self.build_client_snapshot(i);
        let mut m = Msg::new(MAX_MSGLEN);
        m.allowoverflow = true;
        // NOTE, MRE: all server->client messages now acknowledge
        // let the client know which reliable clientCommands we have received
        m.write_long(self.clients[i].last_client_command);
        // (re)send any reliable server commands
        Self::update_server_commands(&mut self.clients[i], &mut m);
        // send over all the relevant entityState_t
        // and the playerState_t
        self.write_snapshot_to_client(i, &mut m);
        // wt-b VoIP: SV_WriteVoipToClient
        let downloading = !self.clients[i].download.name.is_empty();
        self.clients[i].voip.write_to_client(downloading, &mut m);
        // check for overflow
        if m.overflowed {
            self.console.push_str(&format!("WARNING: msg overflowed for {}\n", self.clients[i].name));
            m.clear();
        }
        self.send_message_to_client(net, i, m);
    }

    /// `SV_RateMsec`.
    fn rate_msec(&self, i: usize) -> i32 {
        let c = &self.clients[i];
        let mut rate = c.rate;
        if self.cvars.max_rate != 0 {
            rate = rate.min(self.cvars.max_rate.max(1000));
        }
        if self.cvars.min_rate != 0 {
            rate = rate.max(self.cvars.min_rate.max(1000));
        }
        let size = c.netchan.last_sent_size + if c.netchan.remote_address.kind == NetAdrType::Ip6 { UDPIP6_HEADER_SIZE } else { UDPIP_HEADER_SIZE };
        let rate_msec = (size as i32 * 1000) / rate.max(1);
        let elapsed = now_ms() - c.netchan.last_sent_time;
        if elapsed > rate_msec {
            0
        } else {
            rate_msec - elapsed
        }
    }

    /// `SV_SendClientMessages`.
    fn send_client_messages(&mut self, net: &mut Net) {
        for i in 0..self.clients.len() {
            let c = &self.clients[i];
            if c.state == ClientState::Free {
                continue; // not connected
            }
            if !c.download.name.is_empty() {
                continue; // Client is downloading, don't send snapshots
            }
            if c.netchan.unsent_fragments || !c.netchan_queue.is_empty() {
                self.clients[i].rate_delayed = true;
                continue; // Drop this snapshot if the packet queue is still full or delta compression will break
            }
            let lan = c.netchan.remote_address.kind == NetAdrType::Loopback || (self.cvars.lan_force_rate && netchan::is_lan_address(&c.netchan.remote_address));
            if !lan {
                if self.time - c.last_snapshot_time < c.snapshot_msec {
                    continue; // It's not time yet
                }
                if self.rate_msec(i) > 0 {
                    self.clients[i].rate_delayed = true;
                    continue;
                }
            } else if self.time - c.last_snapshot_time < self.frame_msec() {
                // a LAN client still gets one snapshot per server frame
                continue;
            }
            if c.state == ClientState::Connected {
                continue; // no gamestate yet
            }
            // generate and send a new message
            self.send_client_snapshot(net, i);
            self.clients[i].last_snapshot_time = self.time;
            self.clients[i].rate_delayed = false;
        }
    }

    /// `SV_SendQueuedPackets`: queued messages, fragments and downloads.
    fn send_queued_packets(&mut self, net: &mut Net) {
        // SV_SendQueuedMessages
        for i in 0..self.clients.len() {
            if self.clients[i].state == ClientState::Free {
                continue;
            }
            if self.rate_msec(i) == 0 || netchan::is_lan_address(&self.clients[i].netchan.remote_address) {
                // SV_Netchan_TransmitNextFragment
                let c = &mut self.clients[i];
                if c.netchan.unsent_fragments {
                    c.netchan.transmit_next_fragment(net, now_ms());
                } else if let Some(m) = c.netchan_queue.pop_front() {
                    c.netchan.transmit(net, &m, now_ms());
                }
            }
        }
        // SV_SendDownloadMessages
        for i in 0..self.clients.len() {
            if self.clients[i].state == ClientState::Free || self.clients[i].download.name.is_empty() {
                continue;
            }
            // no download rate cap (sv_dlRate): every block the window allows
            // goes out this frame, each with all of its fragments, instead of
            // one block per frame; only the window and the client's acks pace it
            loop {
                let c = &self.clients[i];
                if c.netchan.unsent_fragments || !c.netchan_queue.is_empty() {
                    break; // something else is queued; it goes first
                }
                let mut m = Msg::new(MAX_MSGLEN);
                m.write_long(self.clients[i].last_client_command);
                if !self.write_download_to_client(i, &mut m) {
                    break;
                }
                m.write_byte(svc::EOF);
                let t = self.time;
                let c = &mut self.clients[i];
                let seq = c.netchan.outgoing_sequence;
                if let Some(slot) = c.download.msg_sent.get_mut(seq as usize % DOWNLOAD_MSG_RING) {
                    *slot = (seq, t);
                }
                c.netchan.transmit(net, m.bytes(), now_ms());
                while c.netchan.unsent_fragments {
                    c.netchan.transmit_next_fragment(net, now_ms());
                }
                if c.download.name.is_empty() {
                    break; // that was a refusal; the download is over
                }
            }
        }
    }

    /// `SV_WriteDownloadToClient`: true when a block was written.
    fn write_download_to_client(&mut self, i: usize, m: &mut Msg) -> bool {
        let t = self.time;
        if self.clients[i].download.data.is_none() {
            let name = self.clients[i].download.name.clone();
            // Chop off filename extension; check for pk3 filename extension:
            // only a pak in the list of referenced paks can be downloaded,
            // to prevent downloading of arbitrary files
            let mut unreferenced = true;
            if let Some((stem, ext)) = name.rsplit_once('.') {
                if ext.eq_ignore_ascii_case("pk3") {
                    let refs = self.fs.as_ref().map(|f| f.referenced_pak_names()).unwrap_or_default();
                    unreferenced = !refs.split_whitespace().any(|r| r.replace('\\', "/").eq_ignore_ascii_case(&stem.replace('\\', "/")));
                }
            }
            let enabled = self.cvars.allow_download & DLF_ENABLE != 0 && self.cvars.allow_download & DLF_NO_UDP == 0;
            let allowed = enabled && !unreferenced && !files::check_dir_traversal(&name);
            let data = if !allowed {
                None
            } else if let Some(f) = self.file_source.as_mut() {
                f(&name)
            } else {
                self.fs.as_ref().and_then(|fs| fs.sv_read_pak(&name))
            };
            match data {
                Some(d) => {
                    self.console.push_str(&format!("clientDownload: {i} : beginning \"{name}\"\n"));
                    let d_ = &mut self.clients[i].download;
                    d_.size = d.len() as i32;
                    d_.data = Some(d);
                    d_.current_block = 0;
                    d_.client_block = 0;
                    d_.xmit_block = 0;
                    d_.count = 0;
                    d_.eof = false;
                    d_.blocks = vec![Vec::new(); MAX_DOWNLOAD_WINDOW];
                    d_.sent_at = vec![0; MAX_DOWNLOAD_WINDOW];
                    d_.msg_sent = vec![(-1, 0); DOWNLOAD_MSG_RING];
                    d_.srtt = 0;
                }
                None => {
                    // cannot auto-download file
                    let msg = if unreferenced {
                        self.console.push_str(&format!("clientDownload: {i} : \"{name}\" is not referenced and cannot be downloaded.\n"));
                        format!("File \"{name}\" is not referenced and cannot be downloaded.")
                    } else if !enabled {
                        self.console.push_str(&format!("clientDownload: {i} : \"{name}\" download disabled\n"));
                        if self.pure_active() {
                            format!("Could not download \"{name}\" because autodownloading is disabled on the server.\n\nYou will need to get this file elsewhere before you can connect to this pure server.\n")
                        } else {
                            format!("Could not download \"{name}\" because autodownloading is disabled on the server.\n\nThe server you are connecting to is not a pure server, set autodownload to No in your settings and you might be able to join the game anyway.\n")
                        }
                    } else {
                        self.console.push_str(&format!("clientDownload: {i} : \"{name}\" file not found on server\n"));
                        format!("File \"{name}\" not found on server for autodownloading.\n")
                    };
                    m.write_byte(svc::DOWNLOAD);
                    m.write_short(0); // client is expecting block zero
                    m.write_long(-1); // illegal file size
                    m.write_string(&msg);
                    self.clients[i].download = Download::default();
                    return true;
                }
            }
        }
        let ping = self.clients[i].ping.clamp(0, 1000);
        let d = &mut self.clients[i].download;
        let data = d.data.as_ref().unwrap();
        // perform any reads that we need to
        while d.current_block - d.client_block < MAX_DOWNLOAD_WINDOW && d.size != d.count {
            let ci = d.current_block % MAX_DOWNLOAD_WINDOW;
            let start = d.count as usize;
            let end = (start + MAX_DOWNLOAD_BLKSIZE).min(data.len());
            d.blocks[ci] = data[start..end].to_vec();
            d.count += (end - start) as i32;
            d.current_block += 1;
        }
        // load in a zero-length block at the end of the file
        if d.count == d.size && !d.eof && d.current_block - d.client_block < MAX_DOWNLOAD_WINDOW {
            let ci = d.current_block % MAX_DOWNLOAD_WINDOW;
            d.blocks[ci] = Vec::new();
            d.current_block += 1;
            d.eof = true; // We have added the EOF block
        }
        if d.client_block == d.current_block {
            return false; // Nothing to transmit
        }
        // the oldest unacknowledged block is overdue: the client lost it (or
        // a fragment of it) and drops every later block, so go back to it.
        // The deadline follows the round trip measured from this download's
        // own acks (the client's ping is not kept until it is active)
        let timeout = if d.srtt > 0 { (d.srtt * 3 / 2 + 30).clamp(60, 1000) } else { DOWNLOAD_RESEND_MSEC + ping };
        // a client that kept the blocks after a gap acknowledges them all at
        // once when the gap is filled, past what was resent so far
        d.xmit_block = d.xmit_block.max(d.client_block);
        if d.client_block < d.xmit_block && t - d.sent_at[d.client_block % MAX_DOWNLOAD_WINDOW] > timeout {
            d.xmit_block = d.client_block;
        }
        if d.xmit_block == d.current_block {
            return false; // the whole window is out; wait for acks
        }
        // send current block
        let ci = d.xmit_block % MAX_DOWNLOAD_WINDOW;
        m.write_byte(svc::DOWNLOAD);
        m.write_short(d.xmit_block as i32);
        // block zero is special, contains file size
        if d.xmit_block == 0 {
            m.write_long(d.size);
        }
        m.write_short(d.blocks[ci].len() as i32);
        // write the block
        if !d.blocks[ci].is_empty() {
            let b = d.blocks[ci].clone();
            m.write_data(&b);
        }
        d.sent_at[ci] = t;
        d.xmit_block += 1;
        d.send_time = t;
        true
    }

    // ============================================================ masters

    /// `SV_MasterHeartbeat` (dedicated 2 only; masters are resolved on a
    /// background thread and an unresolvable one is reported and skipped).
    fn master_heartbeat(&mut self, net: &mut Net, message: &str) {
        if self.cvars.dedicated != 2 || self.time < self.next_heartbeat_time {
            return;
        }
        self.next_heartbeat_time = self.time + HEARTBEAT_MSEC;
        if self.master_rx.is_none() && self.master_adrs.iter().all(|a| a.is_none()) {
            let names = self.cvars.masters.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                for (i, n) in names.iter().enumerate() {
                    if n.is_empty() {
                        continue;
                    }
                    let (r, mut a) = netchan::string_to_adr(n, NetAdrType::Unspec);
                    if r == 2 {
                        a.port = netchan::PORT_MASTER;
                    }
                    let _ = tx.send((i, (r != 0).then_some(a)));
                }
            });
            self.master_rx = Some(rx);
            for n in self.cvars.masters.iter().filter(|n| !n.is_empty()) {
                self.console.push_str(&format!("Resolving {n}\n"));
            }
            // the heartbeat goes out once the addresses are known
            self.next_heartbeat_time = self.time;
            return;
        }
        let mut sent = false;
        for i in 0..MAX_MASTER_SERVERS {
            if let Some(a) = self.master_adrs[i] {
                self.console.push_str(&format!("Sending heartbeat to {}\n", self.cvars.masters[i]));
                net.out_of_band_print(NetSrc::Server, &a, &format!("heartbeat {message}\n"));
                sent = true;
            }
        }
        // the C resolves the masters before sending; while the background
        // resolution has not answered yet, try again next frame (wt-a)
        if !sent && self.master_rx.is_some() && self.cvars.masters.iter().any(|m| !m.is_empty()) {
            self.next_heartbeat_time = self.time;
        }
    }

    fn receive_masters(&mut self) {
        let Some(rx) = self.master_rx.as_ref() else { return };
        let mut got = Vec::new();
        while let Ok(x) = rx.try_recv() {
            got.push(x);
        }
        for (i, a) in got {
            match a {
                Some(a) => {
                    self.console.push_str(&format!("{} resolved to {}\n", self.cvars.masters[i], a.to_string_port()));
                    self.master_adrs[i] = Some(a);
                }
                None => {
                    self.console.push_str(&format!("Couldn't resolve address: {}\n", self.cvars.masters[i]));
                    self.cvars.masters[i].clear();
                }
            }
        }
    }

    /// `SV_MasterShutdown` + `SV_FinalMessage`: tell the masters and every
    /// client that the server is going down.
    pub fn shutdown(&mut self, net: &mut Net, message: &str) {
        if self.cvars.dedicated == 2 {
            for a in self.master_adrs.clone().into_iter().flatten() {
                net.out_of_band_print(NetSrc::Server, &a, &format!("heartbeat {GAMENAME}\n"));
            }
        }
        // send it twice, ignoring rate
        for _ in 0..2 {
            for i in 0..self.clients.len() {
                if self.clients[i].state >= ClientState::Connected {
                    // don't send a disconnect to a local client
                    if self.clients[i].netchan.remote_address.kind != NetAdrType::Loopback {
                        self.send_server_command(Some(i), &format!("print \"{message}\n\"\n"));
                        self.send_server_command(Some(i), &format!("disconnect \"{message}\""));
                    }
                    // force a snapshot to be sent
                    self.clients[i].netchan_queue.clear();
                    self.clients[i].netchan.unsent_fragments = false;
                    self.send_client_snapshot(net, i);
                }
            }
        }
    }

    /// `SV_Heartbeat_f`: the next frame sends one.
    pub fn force_heartbeat(&mut self) {
        self.next_heartbeat_time = -9999999;
    }

    /// `SV_Status_f` (the engine's status command).
    pub fn status(&self) -> String {
        let mut s = format!("map: {}\nnum score ping name            lastmsg address               qport rate\n--- ----- ---- --------------- ------- --------------------- ----- -----\n", self.map_name);
        for (i, c) in self.clients.iter().enumerate() {
            if c.state == ClientState::Free {
                continue;
            }
            let ps = &self.world.clients[i].ps;
            let ping = match c.state {
                ClientState::Connected => "CNCT".to_string(),
                ClientState::Zombie => "ZMBI".to_string(),
                _ => c.ping.min(9999).to_string(),
            };
            s.push_str(&format!("{i:3} {:5} {ping:>4} {:<15} {:7} {:<21} {:5} {:5}\n", ps.persistant[crate::gen::enums::Pers::Score as usize], c.name, self.time - c.last_packet_time, c.netchan.remote_address.to_string_port(), c.netchan.qport, c.rate));
        }
        s
    }

    /// Clients that are in the game over the network.
    pub fn active_clients(&self) -> usize {
        self.clients.iter().filter(|c| c.state == ClientState::Active).count()
    }
}

/// `CM_WriteAreaBits`.
pub fn write_area_bits(w: &World, buffer: &mut [u8; MAX_MAP_AREA_BYTES], area: i32) -> usize {
    let cm = &w.cm;
    let bytes = ((cm.num_areas.max(0) as usize) + 7) >> 3;
    let bytes = bytes.min(MAX_MAP_AREA_BYTES);
    if area == -1 || area >= cm.num_areas {
        // for debugging, send everything
        buffer[..bytes].iter_mut().for_each(|b| *b = 255);
    } else {
        let flood = cm.area_flood[area as usize].0;
        for i in 0..(cm.num_areas.max(0) as usize).min(MAX_MAP_AREA_BYTES * 8) {
            if cm.area_flood[i].0 == flood {
                buffer[i >> 3] |= 1 << (i & 7);
            }
        }
    }
    bytes
}

/// `SV_EmitPacketEntities`: delta from the acknowledged frame (or the
/// baselines), removes for what left the view, then the end marker.
pub fn emit_packet_entities(from: Option<&ClientSnapshot>, to: &ClientSnapshot, baselines: &[Option<EntityState>], m: &mut Msg) {
    let from_ents: &[EntityState] = from.map(|f| f.entities.as_slice()).unwrap_or(&[]);
    let (mut newindex, mut oldindex) = (0usize, 0usize);
    let null = EntityState::default();
    while newindex < to.entities.len() || oldindex < from_ents.len() {
        let newnum = to.entities.get(newindex).map(|e| e.number).unwrap_or(9999);
        let oldnum = from_ents.get(oldindex).map(|e| e.number).unwrap_or(9999);
        if newnum == oldnum {
            // delta update from old position
            // because the force parm is qfalse, this will not result
            // in any bytes being emited if the entity has not changed at all
            m.write_delta_entity(Some(&from_ents[oldindex]), Some(&to.entities[newindex]), false);
            oldindex += 1;
            newindex += 1;
            continue;
        }
        if newnum < oldnum {
            // this is a new entity, send it from the baseline
            let base = baselines.get(newnum as usize).and_then(|b| b.as_ref()).unwrap_or(&null);
            m.write_delta_entity(Some(base), Some(&to.entities[newindex]), true);
            newindex += 1;
            continue;
        }
        // the old entity isn't present in the new message
        m.write_delta_entity(Some(&from_ents[oldindex]), None, true);
        oldindex += 1;
    }
    m.write_bits(MAX_GENTITIES - 1, GENTITYNUM_BITS); // end of packetentities
}

/// `Cmd_Args_Sanitize`: no newlines or command separators inside the
/// arguments the game sees.
pub fn args_sanitize(s: &str) -> String {
    s.chars().map(|c| if matches!(c, '\n' | '\r' | ';') { ' ' } else { c }).collect()
}

/// `trap_RealTime`.
pub fn real_time() -> i32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i32).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gen::maps::MapId;
    use crate::kernel::world::World;

    /// Spoofed sources (every packet a new ipv6 address) must not grow the
    /// rate limit table without bound; a sender already tracked keeps its
    /// own limit.
    #[test]
    fn rate_limit_table_is_bounded() {
        let mut sv = Server::spawn(World::new(MapId::Arena), "arena", ServerCvars::default());
        let me = NetAdr { kind: NetAdrType::Ip, ip: [192, 168, 1, 5], ..Default::default() };
        assert!(!sv.rate_limit_address(&me, 10, 1000));
        for i in 0..100_000u32 {
            let mut ip6 = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            ip6[12..].copy_from_slice(&i.to_be_bytes());
            sv.rate_limit_address(&NetAdr { kind: NetAdrType::Ip6, ip6, ..Default::default() }, 10, 1000);
        }
        assert!(sv.buckets.len() <= MAX_BUCKETS, "{} buckets", sv.buckets.len());
        // the tracked sender keeps its bucket: answered, then limited
        // within one burst (the flood took about a second, so how much of
        // the burst has leaked back depends on the machine)
        let key = { let mut k = [0u8; 16]; k[..4].copy_from_slice(&me.ip); k };
        assert!(sv.buckets.contains_key(&key));
        assert!(!sv.rate_limit_address(&me, 10, 1000));
        assert!((0..11).any(|_| sv.rate_limit_address(&me, 10, 1000)));
    }
}
