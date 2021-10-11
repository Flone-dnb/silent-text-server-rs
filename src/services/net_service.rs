// External.
use aes::Aes128;
use block_modes::block_padding::Pkcs7;
use block_modes::{BlockMode, Ecb};
use bytevec::{ByteDecodable, ByteEncodable};
use chrono::prelude::*;
use num_traits::{cast::ToPrimitive, FromPrimitive};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

// Std.
use std::collections::LinkedList;
use std::io::ErrorKind;
use std::net::*;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

// Custom.
use super::user_tcp_service::*;
use super::user_udp_service::*;
use crate::config_io::*;
use crate::global_params::*;

pub struct UserInfo {
    pub username: String,
    pub room_name: String,
    pub last_ping: u16,
    pub tcp_addr: SocketAddr,
    pub tcp_socket: TcpStream,
    pub udp_socket: Option<UdpSocket>,
    pub tcp_io_mutex: Arc<Mutex<()>>,
    pub last_text_message_sent: DateTime<Local>,
    pub last_time_entered_room: DateTime<Local>,
    pub secret_key: Vec<u8>,
}
impl UserInfo {
    pub fn clone(&self) -> Result<UserInfo, String> {
        let tcp_socket_clone = self.tcp_socket.try_clone();
        if let Err(e) = tcp_socket_clone {
            return Err(format!(
                "UserInfo::clone() failed, error: {} at [{}, {}]",
                e,
                file!(),
                line!()
            ));
        }
        Ok(UserInfo {
            username: self.username.clone(),
            room_name: String::from(DEFAULT_ROOM_NAME),
            last_ping: 0,
            tcp_addr: self.tcp_addr,
            tcp_socket: tcp_socket_clone.unwrap(),
            udp_socket: None,
            tcp_io_mutex: Arc::clone(&self.tcp_io_mutex),
            last_text_message_sent: Local::now(),
            last_time_entered_room: Local::now(),
            secret_key: self.secret_key.clone(),
        })
    }
}

pub enum BanReason {
    WrongPassword,
    Spam,
}

pub struct BannedAddress {
    pub banned_at: DateTime<Local>,
    pub addr: IpAddr,
    pub reason: BanReason,
}

pub struct NetService {
    pub server_config: ServerConfig,
    connected_users: Arc<Mutex<LinkedList<UserInfo>>>,
    logger: Arc<Mutex<ServerLogger>>,
    user_enters_leaves_server_tcp_lock: Arc<Mutex<()>>,
    user_enters_leaves_server_udp_lock: Arc<Mutex<()>>,
    banned_addrs: Arc<Mutex<Option<Vec<BannedAddress>>>>,
    is_running: bool,
}

impl NetService {
    pub fn new() -> Self {
        Self {
            server_config: ServerConfig::new().unwrap(),
            is_running: false,
            connected_users: Arc::new(Mutex::new(LinkedList::new())),
            user_enters_leaves_server_tcp_lock: Arc::new(Mutex::new(())),
            user_enters_leaves_server_udp_lock: Arc::new(Mutex::new(())),
            logger: Arc::new(Mutex::new(ServerLogger::new())),
            banned_addrs: Arc::new(Mutex::new(Some(Vec::new()))),
        }
    }

    pub fn start(&mut self) {
        if self.is_running {
            println!("\nAlready running...");
            return;
        }

        {
            let mut logger_guard = self.logger.lock().unwrap();

            if let Err(e) = logger_guard.open(&self.server_config.log_file_path) {
                println!("ServerLogger::open() failed, error: {}", e);
            }

            if let Err(e) = logger_guard.println_and_log("Starting...") {
                println!("ServerLogger failed, error: {}", e);
            }
        }

        self.is_running = true;

        self.service();
    }

    fn service(&self) {
        let listener_socket =
            TcpListener::bind(format!("0.0.0.0:{}", self.server_config.server_port));

        if let Err(e) = listener_socket {
            println!(
                "listener_socket.accept() failed, error: {} at [{}, {}]",
                e,
                file!(),
                line!()
            );
            return;
        }
        let listener_socket = listener_socket.unwrap();

        {
            let mut logger_guard = self.logger.lock().unwrap();

            if let Err(e) = logger_guard.println_and_log(&format!(
                "Ready. Listening on port {} for connection requests...",
                self.server_config.server_port
            )) {
                println!(
                    "ServerLogger.println_and_log() failed, error: {} at [{}, {}]",
                    e,
                    file!(),
                    line!()
                );
            }
        }

        loop {
            let accept_result = listener_socket.accept();

            if let Err(e) = accept_result {
                println!(
                    "listener_socket.accept() failed, error: {} at [{}, {}]",
                    e,
                    file!(),
                    line!()
                );
                continue;
            }

            let (socket, addr) = accept_result.unwrap();

            // Check if this IP is banned.
            {
                let mut banned_addrs_guard = self.banned_addrs.lock().unwrap();

                // keep only banned in the vec
                // use 'Option' to move out of mutex
                *banned_addrs_guard = Some(
                    banned_addrs_guard
                        .take()
                        .unwrap()
                        .into_iter()
                        .filter(|banned_item| {
                            let time_diff = Local::now() - banned_item.banned_at;
                            time_diff.num_seconds() < PASSWORD_RETRY_DELAY_SEC as i64
                        })
                        .collect::<Vec<BannedAddress>>(),
                );

                // find addr
                let addr_entry = banned_addrs_guard
                    .as_ref()
                    .unwrap()
                    .iter()
                    .position(|banned_item| banned_item.addr == addr.ip());
                if addr_entry.is_some() {
                    continue; // still banned
                }
            }

            if let Err(e) = socket.set_nodelay(true) {
                println!(
                    "socket.set_nodelay() failed on addr ({}), error: {} at [{}, {}]",
                    addr,
                    e,
                    file!(),
                    line!()
                );
                continue;
            }
            if let Err(e) = socket.set_nonblocking(true) {
                println!(
                    "socket.set_nonblocking() failed on addr ({}), error: {} at [{}, {}]",
                    addr,
                    e,
                    file!(),
                    line!()
                );
                continue;
            }

            let user_info = UserInfo {
                username: String::from(""),
                room_name: String::from(DEFAULT_ROOM_NAME),
                last_ping: 0,
                tcp_addr: addr,
                tcp_socket: socket,
                udp_socket: None,
                tcp_io_mutex: Arc::new(Mutex::new(())),
                last_text_message_sent: Local::now(),
                last_time_entered_room: Local::now(),
                secret_key: Vec::new(),
            };

            let logger_copy = Arc::clone(&self.logger);
            let users_copy = Arc::clone(&self.connected_users);
            let config_copy = self.server_config.clone();
            let banned_addrs_copy = Arc::clone(&self.banned_addrs);
            let user_tcp_io_lock_copy = Arc::clone(&self.user_enters_leaves_server_tcp_lock);
            let user_udp_io_lock_copy = Arc::clone(&self.user_enters_leaves_server_udp_lock);
            let server_password_copy = self.server_config.server_password.clone();
            thread::spawn(move || {
                NetService::handle_user(
                    user_info,
                    config_copy,
                    logger_copy,
                    users_copy,
                    banned_addrs_copy,
                    user_tcp_io_lock_copy,
                    user_udp_io_lock_copy,
                    server_password_copy,
                )
            });
        }
    }

    fn handle_user(
        mut user_info: UserInfo,
        server_config: ServerConfig,
        logger: Arc<Mutex<ServerLogger>>,
        users: Arc<Mutex<LinkedList<UserInfo>>>,
        banned_addrs: Arc<Mutex<Option<Vec<BannedAddress>>>>,
        user_enters_leaves_server_tcp_lock: Arc<Mutex<()>>,
        user_enters_leaves_server_udp_lock: Arc<Mutex<()>>,
        server_password: String,
    ) {
        let mut buf_u16 = [0u8; 2];
        let mut _next_packet_size = 0u16;
        let mut is_error = true;
        let mut user_tcp_service = UserTcpService::new();

        match user_tcp_service.establish_secure_connection(&mut user_info) {
            Ok(key) => {
                user_info.secret_key = key;
            }
            Err(_) => {
                // failed
                return;
            }
        }

        let (udp_sender, udp_receiver) = mpsc::channel();
        let udp_receiver = Arc::new(Mutex::new(udp_receiver));

        // Read data from the socket.
        loop {
            // Read 2 bytes.
            match user_tcp_service.read_from_socket(&mut user_info, &mut buf_u16) {
                IoResult::Fin => {
                    is_error = false;
                    break;
                }
                IoResult::WouldBlock => {
                    if user_tcp_service
                        .handle_keep_alive_check(&mut user_info)
                        .is_err()
                    {
                        break;
                    }
                    thread::sleep(Duration::from_millis(INTERVAL_TCP_IDLE_MS));
                    continue;
                }
                IoResult::Err(e) => {
                    println!("{} at [{}, {}]", e, file!(), line!());
                    break;
                }
                IoResult::Ok(_bytes) => {
                    let res = bincode::deserialize(&buf_u16);
                    if let Err(e) = res {
                        println!(
                            "Deserialize error for data size on socket ({}), error: {} at [{}, {}]",
                            user_info.tcp_addr,
                            e,
                            file!(),
                            line!()
                        );
                        break;
                    }

                    _next_packet_size = res.unwrap();
                }
            }

            user_tcp_service.last_keep_alive_check_time = Local::now();
            user_tcp_service.sent_keep_alive = false;

            let prev_state = user_tcp_service.user_state;

            // Using current state and these 2 bytes we know what to do.
            match user_tcp_service.handle_user_state(
                _next_packet_size,
                &mut user_info,
                &server_config,
                &users,
                &banned_addrs,
                &user_enters_leaves_server_tcp_lock,
                &logger,
                &server_password,
            ) {
                HandleStateResult::IoErr(read_e) => match read_e {
                    IoResult::Fin => {
                        is_error = false;
                        break;
                    }
                    IoResult::Err(e) => {
                        println!("{} at [{}, {}]", e, file!(), line!());
                        break;
                    }
                    _ => {}
                },
                HandleStateResult::HandleStateErr(msg) => {
                    println!("{} at [{}, {}]", msg, file!(), line!());
                    break;
                }
                HandleStateResult::UserNotConnectedReason(msg) => {
                    println!("{}", msg);
                    break;
                }
                HandleStateResult::Ok => {}
            };

            if prev_state == UserState::NotConnected
                && user_tcp_service.user_state == UserState::Connected
            {
                // Start UDP service.
                let username_copy = user_info.username.clone();
                let addr_copy = user_info.tcp_addr;
                let users_copy = Arc::clone(&users);
                let r_clone = Arc::clone(&udp_receiver);
                let lock_clone = Arc::clone(&user_enters_leaves_server_udp_lock);
                let secret_key_clone = user_info.secret_key.clone();
                thread::spawn(move || {
                    NetService::udp_service(
                        username_copy,
                        addr_copy,
                        users_copy,
                        r_clone,
                        lock_clone,
                        secret_key_clone,
                    )
                });
            }
        }

        // signal to udp that we are done
        if udp_sender.send(()).is_err() {
            // udp thread probably ended earlier due to error
        }

        let mut _out_str = String::from("");

        if user_tcp_service.user_state == UserState::Connected {
            let mut _users_connected = 0;
            {
                let _guard = user_enters_leaves_server_tcp_lock.lock().unwrap();

                // Erase from global users list.
                let mut users_guard = users.lock().unwrap();
                for (i, user) in users_guard.iter().enumerate() {
                    if user.username == user_info.username {
                        users_guard.remove(i);
                        _users_connected = users_guard.len();
                        break;
                    }
                }
            }

            if is_error {
                _out_str = format!(
                    "Closing connection with socket ({}) AKA ({}) due to error [connected users: {}].",
                    user_info.tcp_addr, user_info.username, _users_connected
                );
            } else {
                _out_str = format!(
                    "Closing connection with socket ({}) AKA ({}) in response to FIN [connected users: {}].",
                    user_info.tcp_addr, user_info.username, _users_connected
                );
            }

            if let HandleStateResult::HandleStateErr(msg) =
                user_tcp_service.send_disconnected_notice(&mut user_info, users)
            {
                println!("{} at [{}, {}]", msg, file!(), line!());
            }
        } else {
            if is_error {
                _out_str = format!(
                    "Closing connection with socket ({}) due to error (this user was not connected).",
                    user_info.tcp_addr,
                );
            } else {
                _out_str = format!(
                    "Closing connection with socket ({}) AKA ({}) in response to FIN (this user was not connected).",
                    user_info.tcp_addr, user_info.username,
                );
            }
        }

        // Show output.
        let mut logger_guard = logger.lock().unwrap();
        if let Err(e) = logger_guard.println_and_log(&_out_str) {
            println!("{} at [{}, {}]", e, file!(), line!());
        }
    }
    fn udp_service(
        username: String,
        addr: SocketAddr,
        users: Arc<Mutex<LinkedList<UserInfo>>>,
        tcp_listen: Arc<Mutex<mpsc::Receiver<()>>>,
        user_connect_disconnect_server_lock: Arc<Mutex<()>>,
        secret_key: Vec<u8>,
    ) {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP));
        if let Err(e) = socket {
            println!(
                "Socket::new() failed, error: {}, at [{}, {}]",
                e,
                file!(),
                line!()
            );
            return;
        }
        let socket = socket.unwrap();
        let sock_addr = SockAddr::from(SocketAddrV4::new(
            Ipv4Addr::new(0, 0, 0, 0),
            SERVER_DEFAULT_PORT,
        ));
        let res = socket.set_reuse_address(true);
        if let Err(e) = res {
            println!(
                "Socket::set_reuse_address() failed, error: {}, at [{}, {}]",
                e,
                file!(),
                line!()
            );
            return;
        }
        if let Err(e) = socket.bind(&sock_addr) {
            println!(
                "Socket::bind() failed, error: {}, at [{}, {}]",
                e,
                file!(),
                line!()
            );
            return;
        }

        let udp_socket: UdpSocket = socket.into();

        if let Err(e) = udp_socket.set_nonblocking(true) {
            println!(
                "udp_socket.set_nonblocking() failed, error: {}, at [{}, {}]",
                e,
                file!(),
                line!()
            );
            return;
        }

        let user_udp_service = UserUdpService::new(secret_key, username);

        // Wait for "connection".
        match user_udp_service.wait_for_connection(
            &udp_socket,
            addr,
            &users,
            &user_connect_disconnect_server_lock,
        ) {
            Ok(()) => {}
            Err(msg) => {
                println!("{} at [{}, {}]", msg, file!(), line!());
                return;
            }
        }

        match user_udp_service.do_first_ping_check(&udp_socket) {
            Ok(ping_ms) => {
                if let Err(e) = user_udp_service.send_user_ping_to_all(ping_ms, &users) {
                    println!("{} at [{}, {}]", e, file!(), line!());
                    return;
                }
            }
            Err(msg) => {
                println!("{} at [{}, {}]", msg, file!(), line!());
                return;
            }
        }

        // // Ready.
        // let mut last_ping_check_time = Local::now();
        // let mut in_buf = vec![0u8; IN_UDP_BUFFER_SIZE];
        // loop {
        //     match udp_socket.recv(&mut in_buf) {
        //         Ok(_) => match FromPrimitive::from_u8(in_buf[0]) {
        //             Some(ClientMessageUdp::PingCheck) => {
        //                 // Update user ping.
        //                 let time_diff = Local::now() - last_ping_check_time;
        //                 let user_ping_ms = time_diff.num_milliseconds() as u16;
        //                 last_ping_check_time = Local::now();

        //                 // Prepare packet about user ping to all.
        //                 let mut ping_info_buf = Vec::new();
        //                 match user_udp_service.prepare_ping_info_buf(&username, &mut ping_info_buf)
        //                 {
        //                     Ok(()) => {}
        //                     Err(msg) => {
        //                         println!("{} at [{}, {}]", msg, file!(), line!());
        //                         return;
        //                     }
        //                 }

        //                 // ping to buf
        //                 let ping_buf = u16::encode::<u16>(&user_ping_ms);
        //                 if let Err(e) = ping_buf {
        //                     println!(
        //                         "u16::encode::<u16>() failed, error: {} at [{}, {}]",
        //                         e,
        //                         file!(),
        //                         line!()
        //                     );
        //                     return;
        //                 }
        //                 let mut ping_buf = ping_buf.unwrap();
        //                 ping_info_buf.append(&mut ping_buf);

        //                 let mut users_guard = users.lock().unwrap();
        //                 for user in users_guard.iter_mut() {
        //                     if user.username == username {
        //                         user.last_ping = user_ping_ms;
        //                     }
        //                     if user.udp_socket.is_some() {
        //                         match user_udp_service
        //                             .send(user.udp_socket.as_ref().unwrap(), &ping_info_buf)
        //                         {
        //                             Ok(()) => {}
        //                             Err(msg) => {
        //                                 println!("{} at [{}, {}]", msg, file!(), line!());
        //                                 return;
        //                             }
        //                         }
        //                     }
        //                 }
        //             }
        //             Some(ClientMessageUdp::VoicePacket) => {
        //                 let mut read_index = 1usize;

        //                 // read voice data (encrypted) len
        //                 let encrypted_voice_data_len_buf =
        //                     &in_buf[1..1 + std::mem::size_of::<u16>()];
        //                 read_index += std::mem::size_of::<u16>();
        //                 let encrypted_voice_data_len =
        //                     u16::decode::<u16>(encrypted_voice_data_len_buf);
        //                 if let Err(e) = encrypted_voice_data_len {
        //                     println!(
        //                         "u16::decode::<u16>() failed, error: {} at [{}, {}]",
        //                         e,
        //                         file!(),
        //                         line!()
        //                     );
        //                     return;
        //                 }
        //                 let encrypted_voice_data_len = encrypted_voice_data_len.unwrap();

        //                 // Read voice data (encrypted)
        //                 let encrypted_voice_data =
        //                     &in_buf[read_index..read_index + encrypted_voice_data_len as usize];

        //                 // Decrypt user voice data.
        //                 type Aes128Ecb = Ecb<Aes128, Pkcs7>;
        //                 let cipher =
        //                     Aes128Ecb::new_from_slices(&secret_key, Default::default()).unwrap();
        //                 let decrypted_message = cipher.decrypt_vec(encrypted_voice_data);
        //                 if let Err(e) = decrypted_message {
        //                     println!(
        //                         "cipher.decrypt_vec() failed, error: {} at [{}, {}]",
        //                         e,
        //                         file!(),
        //                         line!()
        //                     );
        //                     return;
        //                 }
        //                 let user_voice_message = decrypted_message.unwrap();

        //                 // Prepare out packet:
        //                 // (u8) - id (ServerMessageUdp::VoiceMessage)
        //                 // (u8) - username len
        //                 // (size) - username
        //                 // (u16) - voice data len (encrypted)
        //                 // (size) - voice data (encrypted)
        //                 let packet_id = ServerMessageUdp::VoiceMessage.to_u8().unwrap();
        //                 let mut username_buf = Vec::from(username.as_bytes());
        //                 let username_len = username_buf.len() as u8;

        //                 let mut out_buf: Vec<u8> = Vec::new();
        //                 out_buf.push(packet_id);
        //                 out_buf.push(username_len);
        //                 out_buf.append(&mut username_buf);

        //                 let mut users_guard = users.lock().unwrap();
        //                 let mut user_room = String::from(DEFAULT_ROOM_NAME);
        //                 // get current user room
        //                 for user in users_guard.iter() {
        //                     if user.username == username {
        //                         user_room = user.room_name.clone();
        //                         break;
        //                     }
        //                 }

        //                 // send voice message
        //                 for user in users_guard.iter_mut() {
        //                     if user.username != username
        //                         && user.udp_socket.is_some()
        //                         && user.room_name == user_room
        //                     {
        //                         let mut copy_buf = out_buf.clone();

        //                         // Encrypt with user key.
        //                         let cipher = Aes128Ecb::new_from_slices(
        //                             &user.secret_key,
        //                             Default::default(),
        //                         )
        //                         .unwrap();
        //                         let mut encrypted_voice_message =
        //                             cipher.encrypt_vec(&user_voice_message);

        //                         // Prepare message len buffer.
        //                         let encrypted_message_len = encrypted_voice_message.len() as u16;
        //                         let encrypted_message_len_buf =
        //                             u16::encode::<u16>(&encrypted_message_len);
        //                         if let Err(e) = encrypted_message_len_buf {
        //                             println!(
        //                                 "u16::encode::<u16>() failed, error: {} at [{}, {}]",
        //                                 e,
        //                                 file!(),
        //                                 line!()
        //                             );
        //                             return;
        //                         }
        //                         let mut encrypted_message_len_buf =
        //                             encrypted_message_len_buf.unwrap();

        //                         copy_buf.append(&mut encrypted_message_len_buf);
        //                         copy_buf.append(&mut encrypted_voice_message);

        //                         match user_udp_service
        //                             .send(user.udp_socket.as_ref().unwrap(), &copy_buf)
        //                         {
        //                             Ok(()) => {}
        //                             Err(msg) => {
        //                                 println!("{} at [{}, {}]", msg, file!(), line!());
        //                                 return;
        //                             }
        //                         }
        //                     }
        //                 }
        //             }
        //             None => {
        //                 println!(
        //                     "FromPrimitive::from_u8() failed with value {}, at [{}, {}]",
        //                     in_buf[0],
        //                     file!(),
        //                     line!()
        //                 );
        //                 return;
        //             }
        //         },
        //         Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
        //             {
        //                 if tcp_listen.lock().unwrap().try_recv().is_ok() {
        //                     // tcp thread ended, finish this thread
        //                     return;
        //                 }
        //             }

        //             let time_diff = Local::now() - last_ping_check_time;
        //             if time_diff.num_seconds() > INTERVAL_PING_CHECK_SEC {
        //                 match user_udp_service.send_ping_check(&udp_socket) {
        //                     Ok(()) => last_ping_check_time = Local::now(),
        //                     Err(msg) => {
        //                         println!("{}, at [{}, {}]", msg, file!(), line!());
        //                         return;
        //                     }
        //                 }
        //             }

        //             thread::sleep(Duration::from_millis(INTERVAL_UDP_IDLE_MS));
        //             continue;
        //         }
        //         Err(e) => {
        //             println!(
        //                 "udp_socket.recv() failed, error: {}, at [{}, {}]",
        //                 e,
        //                 file!(),
        //                 line!()
        //             );
        //             return;
        //         }
        //     }
        // }
    }
}
