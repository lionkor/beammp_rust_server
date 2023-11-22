use std::net::SocketAddr;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use std::time::Instant;
use std::collections::HashMap;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::task::{JoinHandle, JoinSet};

use num_enum::IntoPrimitive;

use nalgebra::*;

mod backend;
mod car;
mod client;
mod packet;
mod plugins;
mod http;

pub use backend::*;
pub use car::*;
pub use client::*;
pub use packet::*;
pub use plugins::*;
pub use http::*;

pub use crate::config::Config;

fn load_plugins() -> Vec<Plugin> {
    let mut plugins = Vec::new();

    for res_entry in std::fs::read_dir("Resources/Server").expect("Failed to read Resources/Server!") {
        if let Ok(res_entry) = res_entry {
            let res_path = res_entry.path();
            if res_path.is_dir() {

                // TODO: Fix this (split into different functions)
                if let Ok(read_dir) = std::fs::read_dir(&res_path) {
                    for entry in read_dir {
                        if let Ok(entry) = entry {
                            let path = entry.path();
                            if path.is_file() {
                                if let Some(filename) = path.file_name() {
                                    let filename = filename.to_string_lossy().to_string();
                                    let filename = filename.split(".").next().unwrap();
                                    if filename == "main" {
                                        if let Ok(src) = std::fs::read_to_string(&path) {
                                            let extension = path.extension().map(|s| s.to_string_lossy().to_string()).unwrap_or(String::new());
                                            if let Some(backend) = match extension.as_str() {
                                                "lua" => Some(Box::new(backend_lua::BackendLua::new())),
                                                _ => None,
                                            } {
                                                debug!("Loading plugin: {:?}", res_path);
                                                if let Ok(plugin) = Plugin::new(backend, src) {
                                                    plugins.push(plugin);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

            }
        }
    }

    plugins
}

#[derive(PartialEq, Eq, Clone, Debug)]
pub struct ServerStatus {
    pub player_count: usize,
    pub player_list: Vec<(u8, String)>,
    pub max_players: usize,
}

pub async fn read_tcp(clients: &mut Vec<Client>) -> anyhow::Result<Option<(usize, RawPacket)>> {
    let (result, index, _) = futures::future::select_all(
        clients.iter_mut().map(|client| Box::pin(client.process_blocking()))
    ).await;

    Ok(match result {
        Ok(packet_opt) => {
            if let Some(raw_packet) = packet_opt {
                Some((index, raw_packet))
            } else {
                None
            }
        },
        Err(e) => {
            if let Some(client) = clients.get_mut(index) {
                client.kick(&format!("Kicked: {:?}", e)).await;
            }
            None
        }
    })
}

pub async fn read_udp(udp_socket: &UdpSocket) -> Option<(SocketAddr, RawPacket)> {
    let mut data = vec![0u8; 4096];
    let data_size;
    let data_addr;

    match udp_socket.recv_from(&mut data).await {
        Ok((0, _)) => {
            error!("UDP socket is readable, yet has 0 bytes to read!");
            return None;
        }
        Ok((n, addr)) => (data_size, data_addr) = (n, addr),
        Err(_) => return None,
    }

    let packet = RawPacket {
        header: data_size as u32,
        data: data[..data_size].to_vec(),
    };

    Some((data_addr, packet))
}

pub struct Server {
    tcp_listener: Arc<TcpListener>,
    pub udp_socket: Arc<UdpSocket>,

    clients_incoming: Arc<Mutex<Vec<Client>>>,
    clients_queue: Vec<(Client, Vec<tokio::sync::oneshot::Receiver<Argument>>, Vec<Argument>)>,

    pub clients: Vec<Client>,

    connect_runtime_handle: JoinHandle<()>,

    config: Arc<Config>,

    last_plist_update: Instant,

    plugins: Vec<Plugin>,
}

impl Server {
    pub async fn new(config: Arc<Config>) -> anyhow::Result<Self> {
        let config_ref = Arc::clone(&config);

        let port = config.general.port.unwrap_or(48900);

        let tcp_listener = {
            let bind_addr = &format!("0.0.0.0:{}", port);
            Arc::new(TcpListener::bind(bind_addr).await?)
        };
        let tcp_listener_ref = Arc::clone(&tcp_listener);

        let udp_socket = {
            let bind_addr = &format!("0.0.0.0:{}", port);
            Arc::new(UdpSocket::bind(bind_addr).await?)
        };

        // Load existing plugins
        let plugins = load_plugins();

        // Start client runtime
        let clients_incoming = Arc::new(Mutex::new(Vec::new()));
        let clients_incoming_ref = Arc::clone(&clients_incoming);
        debug!("Client acception runtime starting...");
        let connect_runtime_handle = tokio::spawn(async move {
            let mut set = JoinSet::new();
            loop {
                match tcp_listener_ref.accept().await {
                    Ok((mut socket, addr)) => {
                        info!("New client connected: {:?}", addr);

                        let cfg_ref = config_ref.clone();
                        let ci_ref = clients_incoming_ref.clone();

                        set.spawn(async move {
                            socket.set_nodelay(true); // TODO: Is this good?

                            socket.readable().await.expect("Failed to wait for socket to become readable!");
                            let mut tmp = vec![0u8; 1];
                            while socket.peek(&mut tmp).await.expect("Failed to peek socket!") == 0 {}
                            // Authentication works a little differently than normal
                            // Not sure why, but the BeamMP source code shows they
                            // also only read a single byte during authentication
                            socket.read_exact(&mut tmp).await.expect("Failed to read from socket!");
                            let code = tmp[0];

                            match code as char {
                                'C' => {
                                    let mut client = Client::new(socket);
                                    match client.authenticate(&cfg_ref).await {
                                        Ok(is_client) if is_client => {
                                            let mut lock = ci_ref
                                                .lock()
                                                .map_err(|e| error!("{:?}", e))
                                                .expect("Failed to acquire lock on mutex!");
                                            lock.push(client);
                                            drop(lock);
                                        },
                                        Ok(_is_client) => {
                                            debug!("Downloader?");
                                        },
                                        Err(e) => {
                                            error!("Authentication error occured, kicking player...");
                                            error!("{:?}", e);
                                            client.kick("Failed to authenticate player!").await;
                                            // client.disconnect();
                                        }
                                    }
                                },
                                'D' => {
                                    // Download connection (for old protocol)
                                    // This crashes the client after sending over 1 mod.
                                    // I have no idea why, perhaps I'm missing something that I'm supposed to send it.
                                    // TODO: Implement this: https://github.com/BeamMP/BeamMP-Server/blob/master/src/TNetwork.cpp#L775

                                    socket.readable().await;
                                    let mut tmp = [0u8; 1];
                                    socket.read_exact(&mut tmp).await;
                                    let id = tmp[0] as usize;
                                    debug!("[D] HandleDownload connection for client id: {}", id);
                                    let mut sent_mods = Vec::new();
                                    'download: while let Ok(_) = socket.writable().await {
                                        {
                                            let lock = CLIENT_MOD_PROGRESS.lock().await;
                                            if lock.get(&(id as u8)).is_none() { continue; }
                                        }
                                        let mod_id = {
                                            let lock = CLIENT_MOD_PROGRESS.lock().await;
                                            *lock.get(&(id as u8)).unwrap()
                                        };
                                        if sent_mods.contains(&mod_id) { continue; }
                                        debug!("[D] Starting download!");
                                        let mut mod_name = {
                                            if mod_id < 0 {
                                                break 'download;
                                            }
                                            if mod_id as usize >= cfg_ref.mods.len() {
                                                break 'download;
                                            }

                                            let bmod = &cfg_ref.mods[mod_id as usize]; // TODO: This is a bit uhh yeah
                                            debug!("[D] Mod name: {}", bmod.0);

                                            bmod.0.clone()
                                        };

                                        if mod_name.starts_with("/") == false {
                                            mod_name = format!("/{mod_name}");
                                        }

                                        debug!("[D] Starting transfer of mod {mod_name}!");

                                        let mod_path = format!("Resources/Client{mod_name}");
                                        if let Ok(file_data) = std::fs::read(mod_path) {
                                            {
                                                trace!("[D] Sending packets!");
                                                if let Err(e) = socket.write(&file_data[(file_data.len()/2)..]).await {
                                                    error!("{:?}", e);
                                                }
                                                trace!("[D] Packets sent!");
                                            }
                                        }

                                        sent_mods.push(mod_id);
                                    }
                                    debug!("[D] Done!");
                                },
                                'G' => {
                                    // This is probably an HTTP GET request!
                                    let mut tmp = [0u8; 3];
                                    socket.read_exact(&mut tmp).await.expect("Failed to read from socket!");
                                    if tmp[0] as char == 'E' && tmp[1] as char == 'T' && tmp[2] as char == ' ' {
                                        trace!("HTTP GET request found!");
                                        handle_http_get(socket).await;
                                    } else {
                                        trace!("Unknown G packet received, not sure what to do!");
                                    }
                                },
                                _ => {},
                            };
                        });
                    }
                    Err(e) => error!("Failed to accept incoming connection: {:?}", e),
                }

                if set.is_empty() == false {
                    // Because join_next() is cancel safe, we can simply cancel it after N duration
                    // so at worst this client acceptance loop blocks for N duration
                    tokio::select!(
                        _ = set.join_next() => {},
                        _ = tokio::time::sleep(tokio::time::Duration::from_millis(10)) => {},
                    )
                }
            }
        });
        debug!("Client acception runtime started!");

        Ok(Self {
            tcp_listener: tcp_listener,
            udp_socket: udp_socket,

            clients_incoming: clients_incoming,
            clients_queue: Vec::new(),

            clients: Vec::new(),

            connect_runtime_handle: connect_runtime_handle,

            config: config,

            last_plist_update: Instant::now(),

            plugins,
        })
    }

    pub fn get_server_status(&self) -> ServerStatus {
        ServerStatus {
            player_count: self.clients.len(),
            player_list: self.clients.iter().map(|client| {
                (client.id, client.get_name().to_string())
            }).collect(),
            // max_players: self.max_players, // TODO: Support this
            max_players: self.config.general.max_players,
        }
    }

    pub async fn close(mut self) {
        self.connect_runtime_handle.abort();
        for mut client in self.clients.drain(..) {
            client.kick("Server is closing!").await;
        }
        // TODO: We can probably race these with futures::future::select_all?
        for plugin in self.plugins.drain(..) {
            plugin.close().await;
        }
    }

    pub async fn process_tcp(&mut self, index: usize, raw_packet: RawPacket) -> anyhow::Result<()> {
        self.parse_packet(index, raw_packet).await?;

        Ok(())
    }

    pub async fn process_udp(&mut self, addr: SocketAddr, packet: RawPacket) -> anyhow::Result<()> {
        // Process UDP packets
        // TODO: Use a UDP addr -> client ID look up table
        if packet.data.len() == 0 {
            return Ok(()); // what!
        }
        let id = packet.data[0] - 1; // Offset by 1
        let data = packet.data[2..].to_vec();
        let packet_processed = RawPacket {
            header: data.len() as u32,
            data,
        };
        'search: for i in 0..self.clients.len() {
            if self.clients[i].id == id {
                self.parse_packet_udp(i, addr, packet_processed).await?;
                break 'search;
            }
        }

        Ok(())
    }

    async fn process_authenticated_clients(&mut self) -> anyhow::Result<()> {
        // Bit weird, but this is all to avoid deadlocking the server if anything goes wrong
        // with the client acception runtime. If that one locks, the server won't accept
        // more clients, but it will at least still process all other clients
        let mut joined_names = Vec::new();
        if let Ok(mut clients_incoming_lock) = self.clients_incoming.try_lock() { // TODO: Why do I use try_lock here?
            if clients_incoming_lock.len() > 0 {
                trace!(
                    "Accepting {} incoming clients...",
                    clients_incoming_lock.len()
                );
                for i in 0..clients_incoming_lock.len() {
                    let (name, role, is_guest, beammp_id) = {
                        let client = clients_incoming_lock[i]
                            .info
                            .as_ref()
                            .unwrap();
                        (client.username.clone(), client.roles.clone(), client.guest, client.uid.clone())
                    };
                    info!("Welcome {name}!");
                    joined_names.push(name.clone());
                    let mut vrx = Vec::new();
                    for plugin in &self.plugins {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        plugin.send_event(PluginBoundPluginEvent::CallEventHandler((ScriptEvent::OnPlayerAuthenticated { name: name.clone(), role: role.clone(), is_guest, identifiers: PlayerIdentifiers {
                            ip: String::from("not yet implemented"),
                            beammp_id: beammp_id.clone(),
                        } }, Some(tx)))).await;
                        // TODO: This never returns, because it blocks the entire process function
                        //       from running, so it never manages to run the function correctly.
                        // let res = rx.await.unwrap_or(Argument::Number(-1f32));
                        // debug!("res: {:?}", res);
                        vrx.push(rx);
                    }
                    self.clients_queue.push((clients_incoming_lock.swap_remove(i), vrx, Vec::new()));
                }
                trace!("Accepted incoming clients!");
            }
        }

        // Bit scuffed but it just polls the return values until all lua plugins have returned
        // If this blocks, the server is stuck!
        // TODO: Reduce allocations needed here (use swap_remove)
        let mut not_done_clients = Vec::new();
        for (mut client, mut vrx, mut res) in self.clients_queue.drain(..) {
            let mut not_done = Vec::new();
            for mut rx in vrx.drain(..) {
                match rx.try_recv() {
                    Ok(v) => { debug!("hi: {:?}", v); res.push(v); },
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => not_done.push(rx),
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {},
                }
            }
            vrx = not_done;

            if vrx.len() == 0 {
                let mut allowed = true;
                for v in res {
                    match v {
                        Argument::Integer(i) => if i == 1 { allowed = false; },
                        Argument::Number(n) => if n == 1f32 { allowed = false; },
                        Argument::Boolean(b) => if b { allowed = false; },
                        _ => {}, // TODO: Handle this somehow?
                    }
                }
                if allowed {
                    self.clients.push(client);
                } else {
                    // TODO: Custom kick message defined from within lua somehow?
                    // TODO: Kicking the client and then immediately dropping them results in the
                    //       kick message not showing up, instead displaying that the socket closed.
                    client.kick("You are not allowed to join this server!").await;
                }
            } else {
                not_done_clients.push((client, vrx, res));
            }
        }
        self.clients_queue = not_done_clients;

        Ok(())
    }

    async fn process_lua_events(&mut self) -> anyhow::Result<()> {
        // Receive plugin events and process them
        for plugin in &mut self.plugins {
            for event in plugin.get_events() {
                debug!("event: {:?}", event);
                // TODO: Error handling (?)
                match event {
                    ServerBoundPluginEvent::PluginLoaded => plugin.send_event(PluginBoundPluginEvent::CallEventHandler((ScriptEvent::OnPluginLoaded, None))).await,
                    ServerBoundPluginEvent::RequestPlayerCount(responder) => { let _ = responder.send(PluginBoundPluginEvent::PlayerCount(self.clients.len() + self.clients_queue.len())); },
                    ServerBoundPluginEvent::RequestPlayers(responder) => {
                        trace!("request players received");
                        let mut players = HashMap::new();
                        for client in &self.clients {
                            players.insert(client.id, client.get_name().to_string());
                        }
                        trace!("sending player list...");
                        let _ = responder.send(PluginBoundPluginEvent::Players(players));
                        trace!("player list sent");
                    },
                    ServerBoundPluginEvent::RequestPlayerIdentifiers((pid, responder)) => {
                        if let Some(client) = self.clients.iter().find(|client| client.id == pid) {
                            let _ = responder.send(PluginBoundPluginEvent::PlayerIdentifiers(PlayerIdentifiers {
                                ip: String::from("not yet implemented"),
                                beammp_id: client.get_userdata().uid,
                            }));
                        } else {
                            let _ = responder.send(PluginBoundPluginEvent::None);
                        }
                    },
                    _ => {},
                }
            }
        }

        Ok(())
    }

    pub async fn process(&mut self) -> anyhow::Result<()> {
        // In the future, we should find a way to race process_tcp and process_udp
        // because this introduces some latency and isn't great!
        // But technically it works, and keeping the latency low should really make
        // it a non-issue I think.
        // TODO: Handle result
        // tokio::select! {
        //     _ = self.process_udp() => {},
        //     _ = tokio::time::sleep(tokio::time::Duration::from_nanos(1_000)) => {},
        // };
        //
        // tokio::select! {
        //     _ = tokio::time::sleep(tokio::time::Duration::from_nanos(1_000)) => {},
        //     _ = self.process_tcp() => {},
        // };

        self.process_authenticated_clients().await?;
        self.process_lua_events().await?;

        // I'm sorry for this code :(
        // TODO: Clean this up. We should just grab the client once with `if let Some() = expr {}`
        for i in 0..self.clients.len() {
            if self.clients.get(i).ok_or(ServerError::ClientDoesntExist)?.state == ClientState::Disconnect {
                let id = self.clients.get(i).ok_or(ServerError::ClientDoesntExist)?.id;
                for j in 0..self.clients.get(i).ok_or(ServerError::ClientDoesntExist)?.cars.len() {
                    let car_id = self.clients.get(i).ok_or(ServerError::ClientDoesntExist)?.cars[j].0;
                    let delete_packet = format!("Od:{}-{}", id, car_id);
                    self.broadcast(Packet::Raw(RawPacket::from_str(&delete_packet)), None)
                        .await;
                }

                let name = self.clients.get(i).ok_or(ServerError::ClientDoesntExist)?.get_name().to_string();
                for plugin in &mut self.plugins {
                    plugin.send_event(PluginBoundPluginEvent::CallEventHandler((ScriptEvent::OnPlayerDisconnect { pid: id, name: name.clone() }, None))).await;
                }

                {
                    let mut lock = CLIENT_MOD_PROGRESS.lock().await;
                    lock.insert(id, -1);
                }

                info!("Disconnecting client {}...", id);
                if i == self.clients.len() - 1 {
                    self.clients.remove(i);
                } else {
                    self.clients.swap_remove(i);
                }
                info!("Client {} disconnected!", id);
            }
        }

        // Update the player list
        if self.last_plist_update.elapsed().as_secs() >= 1 {
            self.last_plist_update = Instant::now();

            let mut players = String::new();

            for client in &self.clients {
                players.push_str(&format!("{},", client.get_name()));
            }

            if players.ends_with(",") {
                players.remove(players.len() - 1);
            }

            let player_count = self.clients.len();
            let max_players = self.config.general.max_players;

            let data = format!("Ss{player_count}/{max_players}:{players}");

            self.broadcast(Packet::Raw(RawPacket::from_str(&data)), None).await;
        }

        Ok(())
    }

    // NOTE: Skips all clients that are currently connecting or syncing resources!
    async fn broadcast(&self, packet: Packet, owner: Option<u8>) {
        for client in &self.clients {
            if let Some(id) = owner {
                if id == client.id {
                    continue;
                }
            }
            if client.state == ClientState::Connecting || client.state == ClientState::SyncingResources {
                continue;
            }
            client.queue_packet(packet.clone()).await;
        }
    }

    async fn broadcast_udp(&self, packet: Packet, owner: Option<u8>) {
        for client in &self.clients {
            if let Some(id) = owner {
                if id == client.id {
                    continue;
                }
            }
            // client.queue_packet(packet.clone()).await;
            if let Some(udp_addr) = client.udp_addr {
                self.send_udp(udp_addr, &packet).await;
            }
        }
    }

    async fn send_udp(&self, udp_addr: SocketAddr, packet: &Packet) {
        let data = packet.get_data();
        if data.len() > 400 {
            trace!("Compressing...");
            let mut compressed: Vec<u8> = Vec::with_capacity(100_000);
            let mut compressor = flate2::Compress::new(flate2::Compression::best(), true);
            if let Err(e) = compressor.compress_vec(
                data,
                &mut compressed,
                flate2::FlushCompress::Sync,
            ) {
                error!("Compression failed!");
                return;
            }
            let mut new_data = "ABG:".as_bytes()[..4].to_vec();
            new_data.append(&mut compressed);
            if let Err(e) = self.udp_socket.try_send_to(&new_data, udp_addr) {
                error!("UDP Packet send error: {:?}", e);
            }
        } else {
            if let Err(e) = self.udp_socket.try_send_to(&data, udp_addr) {
                error!("UDP Packet send error: {:?}", e);
            }
        }
    }

    async fn read_udp_packets(&self) -> Vec<(SocketAddr, RawPacket)> {
        let mut packets = Vec::new();
        'read: loop {
            let mut data = vec![0u8; 4096];
            let data_size;
            let data_addr;

            match self.udp_socket.try_recv_from(&mut data) {
                Ok((0, _)) => {
                    error!("UDP socket is readable, yet has 0 bytes to read!");
                    break 'read;
                }
                Ok((n, addr)) => (data_size, data_addr) = (n, addr),
                Err(_) => break 'read,
            }

            let packet = RawPacket {
                header: data_size as u32,
                data: data[..data_size].to_vec(),
            };
            packets.push((data_addr, packet));
        }
        packets
    }

    async fn read_udp_packets_blocking(&self) -> Option<(SocketAddr, RawPacket)> {
        let mut data = vec![0u8; 4096];
        let data_size;
        let data_addr;

        match self.udp_socket.recv_from(&mut data).await {
            Ok((0, _)) => {
                error!("UDP socket is readable, yet has 0 bytes to read!");
                return None;
            }
            Ok((n, addr)) => (data_size, data_addr) = (n, addr),
            Err(_) => return None,
        }

        let packet = RawPacket {
            header: data_size as u32,
            data: data[..data_size].to_vec(),
        };

        Some((data_addr, packet))
    }

    async fn parse_packet_udp(
        &mut self,
        client_idx: usize,
        udp_addr: SocketAddr,
        mut packet: RawPacket,
    ) -> anyhow::Result<()> {
        if packet.data.len() > 0 {
            let client = &mut self.clients[client_idx];
            let client_id = client.get_id();

            client.udp_addr = Some(udp_addr);

            // Check if compressed
            let mut is_compressed = false;
            if packet.data.len() > 3 {
                let string_data = String::from_utf8_lossy(&packet.data[..4]);
                if string_data.starts_with("ABG:") {
                    is_compressed = true;
                    trace!("Packet is compressed!");
                }
            }

            if is_compressed {
                let compressed = &packet.data[4..];
                let mut decompressed: Vec<u8> = Vec::with_capacity(100_000);
                let mut decompressor = flate2::Decompress::new(true);
                decompressor.decompress_vec(
                    compressed,
                    &mut decompressed,
                    flate2::FlushDecompress::Finish,
                )?;
                packet.header = decompressed.len() as u32;
                packet.data = decompressed;
                // let string_data = String::from_utf8_lossy(&packet.data[..]);
                // debug!("Unknown packet - String data: `{}`; Array: `{:?}`; Header: `{:?}`", string_data, packet.data, packet.header);
            }

            // Check packet identifier
            let packet_identifier = packet.data[0] as char;
            if packet.data[0] >= 86 && packet.data[0] <= 89 {
                self.broadcast_udp(Packet::Raw(packet), Some(client_id))
                    .await;
            } else {
                match packet_identifier {
                    'p' => {
                        self.send_udp(udp_addr, &Packet::Raw(RawPacket::from_code('p')))
                            .await;
                    }
                    'Z' => {
                        if packet.data.len() < 7 {
                            error!("Position packet too small!");
                            return Err(ServerError::BrokenPacket.into());
                        } else {
                            // Sent as text so removing 48 brings it from [48-57] to [0-9]
                            if packet.data[3] < 48 || packet.data[5] < 48 {
                                return Err(ServerError::BrokenPacket.into());
                            }
                            let client_id = packet.data[3] - 48;
                            let car_id = packet.data[5] - 48;

                            let pos_json = &packet.data[7..];
                            let pos_data: TransformPacket =
                                serde_json::from_str(&String::from_utf8_lossy(pos_json))?;

                            let p = Packet::Raw(packet);

                            for i in 0..self.clients.len() {
                                if self.clients[i].id == client_id {
                                    let client = &mut self.clients[i];
                                    let car = client
                                        .get_car_mut(car_id)
                                        .ok_or(ServerError::CarDoesntExist)?;
                                    car.pos = pos_data.pos.into();
                                    car.rot = Quaternion::new(
                                        pos_data.rot[3],
                                        pos_data.rot[0],
                                        pos_data.rot[1],
                                        pos_data.rot[2],
                                    );
                                    car.vel = pos_data.vel.into();
                                    car.rvel = pos_data.rvel.into();
                                    car.tim = pos_data.tim;
                                    car.ping = pos_data.ping;
                                    car.last_pos_update = Some(Instant::now());
                                } else {
                                    if let Some(udp_addr) = self.clients[i].udp_addr {
                                        self.send_udp(udp_addr, &p).await;
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        let string_data = String::from_utf8_lossy(&packet.data[..]);
                        debug!(
                            "Unknown packet UDP - String data: `{}`; Array: `{:?}`; Header: `{:?}`",
                            string_data, packet.data, packet.header
                        );
                    }
                }
            }
        }
        Ok(())
    }

    async fn parse_packet(
        &mut self,
        client_idx: usize,
        mut packet: RawPacket,
    ) -> anyhow::Result<()> {
        if packet.data.len() > 0 {
            let client_id = {
                let client = &mut self.clients[client_idx];
                client.get_id()
            };

            // Check if compressed
            let mut is_compressed = false;
            if packet.data.len() > 3 {
                let string_data = String::from_utf8_lossy(&packet.data[..4]);
                if string_data.starts_with("ABG:") {
                    is_compressed = true;
                    // trace!("Packet is compressed!");
                }
            }

            if is_compressed {
                let compressed = &packet.data[4..];
                let mut decompressed: Vec<u8> = Vec::with_capacity(100_000);
                let mut decompressor = flate2::Decompress::new(true);
                decompressor.decompress_vec(
                    compressed,
                    &mut decompressed,
                    flate2::FlushDecompress::Finish,
                )?;
                packet.header = decompressed.len() as u32;
                packet.data = decompressed;
                // let string_data = String::from_utf8_lossy(&packet.data[..]);
                // debug!("Unknown packet - String data: `{}`; Array: `{:?}`; Header: `{:?}`", string_data, packet.data, packet.header);
            }

            // Check packet identifier
            if packet.data[0] >= 86 && packet.data[0] <= 89 {
                self.broadcast(Packet::Raw(packet), Some(client_id)).await;
            } else {
                let packet_identifier = packet.data[0] as char;
                match packet_identifier {
                    'H' => {
                        // Full sync with server
                        self.clients[client_idx]
                            .queue_packet(Packet::Raw(RawPacket::from_str(&format!(
                                "Sn{}",
                                self.clients[client_idx]
                                    .info
                                    .as_ref()
                                    .unwrap()
                                    .username
                                    .clone()
                            ))))
                            .await;

                        // TODO: Sync all existing cars on server (this code is broken)
                        for client in &self.clients {
                            let pid = client.id as usize;
                            if pid != client_idx {
                                let role = client.get_roles();
                                for (vid, car) in &client.cars {
                                    self.clients[client_idx]
                                        .queue_packet(Packet::Raw(RawPacket::from_str(&format!(
                                            "Os:{role}:{}:{pid}-{vid}:{}",
                                            client.get_name(),
                                            car.car_json,
                                        ))))
                                        .await;
                                }
                            }
                        }
                    }
                    'O' => self.parse_vehicle_packet(client_idx, packet).await?,
                    'C' => {
                        let playername = &self.clients[client_idx].info.as_ref().unwrap().username;
                        let packet_data = packet.data_as_string();
                        let contents: Vec<&str> = packet_data.split(":").collect();
                        if contents.len() < 3 {
                            error!("Message Error - Message from `{}` is of invalid format", &playername);
                            return Ok(());
                        }
                        if contents[1] != playername {
                            error!("Message Error - `{}` is trying to send chat messages for another player `{}`", &playername, &contents[1]);
                            return Ok(());
                        }

                        info!("[CHAT] {}", packet.data_as_string());
                        self.broadcast(Packet::Raw(packet), None).await;
                    }
                    _ => {
                        let string_data = String::from_utf8_lossy(&packet.data[..]);
                        debug!(
                            "Unknown packet - String data: `{}`; Array: `{:?}`; Header: `{:?}`",
                            string_data, packet.data, packet.header
                        );
                    }
                }
            }
        }
        Ok(())
    }

    async fn parse_vehicle_packet(
        &mut self,
        client_idx: usize,
        packet: RawPacket,
    ) -> anyhow::Result<()> {
        if packet.data.len() < 6 {
            error!("Vehicle packet too small!");
            return Ok(()); // TODO: Return error here
        }
        let code = packet.data[1] as char;
        match code {
            's' => {
                let client = &mut self.clients[client_idx];
                let mut allowed = true;
                if let Some(max_cars) = self.config.general.max_cars {
                    if client.cars.len() >= max_cars as usize { allowed = false; }
                }
                // trace!("Packet string: `{}`", packet.data_as_string());
                let split_data = packet
                    .data_as_string()
                    .splitn(3, ':')
                    .map(|s| s.to_string())
                    .collect::<Vec<String>>();
                let car_json_str = &split_data.get(2).ok_or(std::fmt::Error)?;
                // let car_json: serde_json::Value = serde_json::from_str(&car_json_str)?;
                let car_id = client.register_car(Car::new(car_json_str.to_string()));
                let client_id = client.get_id();
                if allowed {
                    let packet_data = format!(
                        "Os:{}:{}:{}-{}:{}",
                        client.get_roles(),
                        client.get_name(),
                        client_id,
                        car_id,
                        car_json_str
                    );
                    let response = RawPacket::from_str(&packet_data);
                    self.broadcast(Packet::Raw(response), None).await;
                    info!("Spawned car for client #{}!", client_id);
                } else {
                    let packet_data = format!(
                        "Os:{}:{}:{}-{}:{}",
                        client.get_roles(),
                        client.get_name(),
                        client_id,
                        car_id,
                        car_json_str
                    );
                    let response = RawPacket::from_str(&packet_data);
                    client.write_packet(Packet::Raw(response)).await;
                    let packet_data = format!(
                        "Od:{}-{}",
                        client_id,
                        car_id,
                    );
                    let response = RawPacket::from_str(&packet_data);
                    client.write_packet(Packet::Raw(response)).await;
                    client.unregister_car(car_id);
                    info!("Blocked spawn for client #{}!", client_id);
                }
            }
            'c' => {
                // let split_data = packet.data_as_string().splitn(3, ':').map(|s| s.to_string()).collect::<Vec<String>>();
                // let car_json_str = &split_data.get(2).ok_or(std::fmt::Error)?;
                let client_id = packet.data[3] - 48;
                let car_id = packet.data[5] - 48;
                let car_json = String::from_utf8_lossy(&packet.data[7..]).to_string();
                let response = Packet::Raw(packet.clone());
                for i in 0..self.clients.len() {
                    if self.clients[i].id == client_id {
                        if let Some(car) = self.clients[i].get_car_mut(car_id) {
                            car.car_json = car_json.clone();
                        }
                    } else {
                        // Already looping so more efficient to send here
                        // if let Some(udp_addr) = self.clients[i].udp_addr {
                        //     self.write_udp(udp_addr, &response).await;
                        // }
                        self.clients[i].write_packet(response.clone()).await;
                    }
                }
            }
            'd' => {
                debug!("packet: {:?}", packet);
                let split_data = packet
                    .data_as_string()
                    .splitn(3, [':', '-'])
                    .map(|s| s.to_string())
                    .collect::<Vec<String>>();
                let client_id = split_data[1].parse::<u8>()?;
                let car_id = split_data[2].parse::<u8>()?;
                for i in 0..self.clients.len() {
                    if self.clients[i].id == client_id {
                        self.clients[i].unregister_car(car_id);
                    }
                    // Don't broadcast, we are already looping anyway
                    // if let Some(udp_addr) = self.clients[i].udp_addr {
                    //     self.send_udp(udp_addr, &Packet::Raw(packet.clone())).await;
                    // }
                    self.clients[i].write_packet(Packet::Raw(packet.clone())).await;
                }
                info!("Deleted car for client #{}!", client_id);
            }
            'r' => {
                self.broadcast(Packet::Raw(packet), Some(self.clients[client_idx].id)).await;
            }
            't' => {
                self.broadcast(Packet::Raw(packet), Some(self.clients[client_idx].id))
                    .await;
            }
            'm' => {
                self.broadcast(Packet::Raw(packet), None).await;
            }
            _ => error!("Unknown vehicle related packet!\n{:?}", packet), // TODO: Return error here
        }
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Not sure how needed this is but it seems right?
        self.connect_runtime_handle.abort();
    }
}

#[derive(Debug)]
pub enum ServerError {
    BrokenPacket,
    CarDoesntExist,
    ClientDoesntExist,
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "{:?}", self)?;
        Ok(())
    }
}

impl std::error::Error for ServerError {}
