use crate::packet::TCPPacket;
use crate::socket::{RetransmissionQueueEntry, SentTime, SockID, Socket, TcpStatus, RTO};
use crate::tcpflags;
use anyhow::{Context, Result};
use pnet::packet::{ip::IpNextHeaderProtocols, tcp::TcpPacket, Packet};
use pnet::transport::{self, TransportChannelType};
use rand::{rngs::ThreadRng, Rng};
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr};
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex, RwLock, RwLockWriteGuard};
use std::time::{Duration, SystemTime};
use std::{cmp, ops::Range, str, thread};

const UNDETERMINED_IP_ADDR: std::net::Ipv4Addr = Ipv4Addr::new(0, 0, 0, 0);
const UNDETERMINED_PORT: u16 = 0;
const MAX_TRANSMISSION: u8 = 5;
const MSS: usize = 1460;
const PORT_RANGE: Range<u16> = 40000..60000;
const WINDOW_PROBE_DURATION: Duration = Duration::from_millis(5000);
const TURN_AROUND_TIMES_MAXLEN: usize = 16;
const RTO_MARGIN: f32 = 3.0;

pub struct TCP {
    sockets: RwLock<HashMap<SockID, Socket>>,
    event_condvar: (Mutex<Option<TCPEvent>>, Condvar),
}

#[derive(Debug, Clone, PartialEq)]
struct TCPEvent {
    sock_id: SockID,
    kind: TCPEventKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TCPEventKind {
    ConnectionCompleted,
    Acked,
    DataArrived,
    ConnectionClosed,
}

impl TCP {
    pub fn new() -> Arc<Self> {
        let sockets = RwLock::new(HashMap::new());
        let tcp = Arc::new(Self {
            sockets,
            event_condvar: (Mutex::new(None), Condvar::new()),
        });

        let cloned_tcp = tcp.clone();
        // 別スレッドで受信ハンドラの処理を行うようにする
        // スリーウェイハンドシェイクのSYNACKの処理もここで行う
        std::thread::spawn(move || {
            cloned_tcp.receive_handler().unwrap();
        });

        let cloned_tcp = tcp.clone();
        //再送管理用のタイマスレッド
        std::thread::spawn(move || {
            cloned_tcp.timer();
        });

        tcp
    }

    fn select_unused_port(&self, rng: &mut ThreadRng) -> Result<u16> {
        for _ in 0..(PORT_RANGE.end - PORT_RANGE.start) {
            let local_port = rng.gen_range(PORT_RANGE);
            let table = self.sockets.read().unwrap();
            if table.keys().all(|k| local_port != k.2) {
                return Ok(local_port);
            }
        }

        anyhow::bail!("no available port found.");
    }

    pub fn connect(&self, addr: Ipv4Addr, port: u16) -> Result<SockID> {
        let mut rng = rand::thread_rng();
        let mut socket = Socket::new(
            get_source_addr_to(addr)?,
            addr,
            self.select_unused_port(&mut rng)?,
            port,
            TcpStatus::SynSent,
        )?;

        socket.send_param.initial_seq = rng.gen_range(1..1 << 31);
        socket.send_tcp_packet(socket.send_param.initial_seq, 0, tcpflags::SYN, &[])?;
        socket.send_param.unacked_seq = socket.send_param.initial_seq;
        socket.send_param.next = socket.send_param.initial_seq + 1;

        let mut table = self.sockets.write().unwrap();
        let sock_id = socket.get_sock_id();
        table.insert(sock_id, socket);

        drop(table);

        // コネクションが確立されるまで待機
        // 接続先から帰ってきたSYNACKの処理などはnew()で作成した受信ハンドラのスレッドで実施する。
        // 受信ハンドラ内でConnectionCompletedが送信されるので、それまで待機することになる。
        self.wait_event(sock_id, TCPEventKind::ConnectionCompleted);

        Ok(sock_id)
    }

    pub fn listen(&self, local_addr: Ipv4Addr, local_port: u16) -> Result<SockID> {
        let socket = Socket::new(
            local_addr,
            UNDETERMINED_IP_ADDR,
            local_port,
            UNDETERMINED_PORT,
            TcpStatus::Listen,
        )?;

        let mut lock = self.sockets.write().unwrap();
        let sock_id = socket.get_sock_id();
        lock.insert(sock_id, socket);

        Ok(sock_id)
    }

    pub fn accept(&self, sock_id: SockID) -> Result<SockID> {
        self.wait_event(sock_id, TCPEventKind::ConnectionCompleted);

        let mut table = self.sockets.write().unwrap();
        Ok(table
            .get_mut(&sock_id)
            .context(format!("no such socket: {:?}", sock_id))?
            .connected_connection_queue
            .pop_front()
            .context("no connected socket")?)
    }

    pub fn send(&self, sock_id: SockID, buffer: &[u8]) -> Result<()> {
        let mut cursor = 0;
        while cursor < buffer.len() {
            let mut table = self.sockets.write().unwrap();
            let mut socket = table
                .get_mut(&sock_id)
                .context(format!("no such socket: {:?}", sock_id))?;
            let send_size = cmp::min(
                MSS,
                cmp::min(socket.send_param.remain() as usize, buffer.len() - cursor),
            );

            if send_size == 0 || socket.last_time_window_probe.is_some() {
                drop(table);
                thread::sleep(Duration::from_millis(1));
                continue;
            }

            dbg!("current window size", socket.send_param.window);

            socket.sent_times.push_back(SentTime {
                sent_time: SystemTime::now(),
                expected_ack: socket.send_param.next + send_size as u32,
            });

            dbg!(socket.send_param.next - socket.send_param.initial_seq);

            // RFC793によるとデータを送るときはACKが必要っぽい
            socket.send_tcp_packet(
                socket.send_param.next,
                socket.recv_param.next,
                tcpflags::ACK,
                &buffer[cursor..cursor + send_size],
            )?;

            cursor += send_size;
            socket.send_param.next += send_size as u32;
            dbg!(socket.send_param.next - socket.send_param.initial_seq);

            // 1msだけtableのロックを解除して受信スレッドが扱えるようにする。
            // 受信スレッドの処理によってwindowの空きを増やすのが狙い
            drop(table);
            thread::sleep(Duration::from_millis(1));
        }

        Ok(())
    }

    pub fn recv(&self, sock_id: SockID, buffer: &mut [u8]) -> Result<usize> {
        let mut table = self.sockets.write().unwrap();
        let mut socket = table
            .get_mut(&sock_id)
            .context(format!("no such socket {:?}", sock_id))?;
        let mut received_size = socket.recv_buffer.len() - socket.recv_param.window as usize;
        while received_size == 0 {
            // すでにFINを受信している場合は待機せずスキップ
            if matches!(
                socket.status,
                TcpStatus::CloseWait | TcpStatus::LastAck | TcpStatus::TimeWait
            ) {
                break;
            }

            drop(table);
            dbg!("waiting incoming data");
            self.wait_event(sock_id, TCPEventKind::DataArrived);
            table = self.sockets.write().unwrap();
            socket = table
                .get_mut(&sock_id)
                .context(format!("no such socket: {:?}", sock_id))?;
            received_size = socket.recv_buffer.len() - socket.recv_param.window as usize;
        }

        let copy_size = cmp::min(buffer.len(), received_size);
        buffer[..copy_size].copy_from_slice(&socket.recv_buffer[..copy_size]);
        socket.recv_buffer.copy_within(copy_size.., 0);
        socket.recv_param.window += copy_size as u16;

        Ok(copy_size)
    }

    pub fn close(&self, sock_id: SockID) -> Result<()> {
        let mut table = self.sockets.write().unwrap();
        let mut socket = table
            .get_mut(&sock_id)
            .context(format!("no such socket: {:?}", sock_id))?;
        socket.send_tcp_packet(
            socket.send_param.next,
            socket.recv_param.next,
            tcpflags::FIN | tcpflags::ACK,
            &[],
        )?;

        socket.send_param.next += 1;

        match socket.status {
            TcpStatus::Established => {
                socket.status = TcpStatus::FinWait1;
                drop(table);
                self.wait_event(sock_id, TCPEventKind::ConnectionClosed);
                let mut table = self.sockets.write().unwrap();
                table.remove(&sock_id);
                dbg!("closed & removed", sock_id);
            }
            TcpStatus::CloseWait => {
                socket.status = TcpStatus::LastAck;
                drop(table);
                self.wait_event(sock_id, TCPEventKind::ConnectionClosed);
                let mut table = self.sockets.write().unwrap();
                table.remove(&sock_id);
                dbg!("closed & removed", sock_id);
            }
            TcpStatus::Listen => {
                table.remove(&sock_id);
            }
            _ => {
                // 上記以外の場合、何もしない
            }
        }

        Ok(())
    }

    fn receive_handler(&self) -> Result<()> {
        dbg!("begin recv thread");

        let (_, mut receiver) = transport::transport_channel(
            65535,
            TransportChannelType::Layer3(IpNextHeaderProtocols::Tcp),
        )
        .unwrap();

        let mut packet_iter = transport::ipv4_packet_iter(&mut receiver);
        loop {
            let (packet, remote_addr) = match packet_iter.next() {
                Ok((p, r)) => (p, r),
                Err(_) => continue,
            };

            let local_addr = packet.get_destination();
            let tcp_packet = match TcpPacket::new(packet.payload()) {
                Some(p) => p,
                None => continue,
            };

            let packet = TCPPacket::from(tcp_packet);
            let remote_addr = match remote_addr {
                IpAddr::V4(addr) => addr,
                _ => continue,
            };

            let mut table = self.sockets.write().unwrap();
            let socket = match table.get_mut(&SockID(
                local_addr,
                remote_addr,
                packet.get_dst(),
                packet.get_src(),
            )) {
                Some(socket) => socket,
                None => match table.get_mut(&SockID(
                    local_addr,
                    UNDETERMINED_IP_ADDR,
                    packet.get_dst(),
                    UNDETERMINED_PORT,
                )) {
                    Some(socket) => socket,
                    None => continue,
                },
            };

            if !packet.is_correct_checksum(local_addr, remote_addr) {
                dbg!("invalid checksum");
                continue;
            }

            let sock_id = socket.get_sock_id();
            if let Err(error) = match socket.status {
                TcpStatus::Listen => self.listen_handler(table, sock_id, &packet, remote_addr),
                TcpStatus::SynRcvd => self.synrcvd_handler(table, sock_id, &packet),
                TcpStatus::SynSent => self.synsent_handler(socket, &packet),
                TcpStatus::Established => self.established_handler(socket, &packet),
                TcpStatus::CloseWait | TcpStatus::LastAck => self.close_handler(socket, &packet),
                TcpStatus::FinWait1 | TcpStatus::FinWait2 => self.finwait_handler(socket, &packet),
                _ => {
                    dbg!("not implemented state");
                    Ok(())
                }
            } {
                dbg!(error);
            }
        }
    }

    fn listen_handler(
        &self,
        mut table: RwLockWriteGuard<HashMap<SockID, Socket>>,
        listening_socket_id: SockID,
        packet: &TCPPacket,
        remote_addr: Ipv4Addr,
    ) -> Result<()> {
        dbg!("listen handler");

        if packet.get_flag() & tcpflags::ACK > 0 {
            // 本来はSYNが来るはずなのでRSTを送るが、今回はRSTは排除するのでOk(())にしている
            return Ok(());
        }

        let listening_socket = table.get_mut(&listening_socket_id).unwrap();

        if packet.get_flag() & tcpflags::SYN > 0 {
            let mut connection_socket = Socket::new(
                listening_socket.local_addr,
                remote_addr,
                listening_socket.local_port,
                packet.get_src(),
                TcpStatus::SynRcvd,
            )?;

            connection_socket.recv_param.next = packet.get_seq() + 1;
            connection_socket.recv_param.initial_seq = packet.get_seq();

            connection_socket.send_param.initial_seq = rand::thread_rng().gen_range(1..1 << 31);
            connection_socket.send_param.window = packet.get_window_size();
            connection_socket.send_tcp_packet(
                connection_socket.send_param.initial_seq,
                connection_socket.recv_param.next,
                tcpflags::SYN | tcpflags::ACK,
                &[],
            )?;

            connection_socket.send_param.next = connection_socket.send_param.initial_seq + 1;
            connection_socket.send_param.unacked_seq = connection_socket.send_param.initial_seq;
            connection_socket.listening_socket = Some(listening_socket.get_sock_id());

            dbg!("status: listen ->", &connection_socket.status);

            table.insert(connection_socket.get_sock_id(), connection_socket);
        }

        Ok(())
    }

    fn synrcvd_handler(
        &self,
        mut table: RwLockWriteGuard<HashMap<SockID, Socket>>,
        sock_id: SockID,
        packet: &TCPPacket,
    ) -> Result<()> {
        dbg!("synrcvd handler");
        let socket = table.get_mut(&sock_id).unwrap();

        if packet.get_flag() & tcpflags::ACK > 0
            && socket.send_param.unacked_seq <= packet.get_ack()
            && packet.get_ack() <= socket.send_param.next
        {
            // packet.get_seq() + 1じゃなくても良い？
            socket.recv_param.next = packet.get_seq();

            socket.send_param.unacked_seq = packet.get_ack();
            socket.status = TcpStatus::Established;
            dbg!("status: synrcvd ->", &socket.status);

            if let Some(id) = socket.listening_socket {
                let ls = table.get_mut(&id).unwrap();
                ls.connected_connection_queue.push_back(sock_id);
                self.publish_event(ls.get_sock_id(), TCPEventKind::ConnectionCompleted);
            }
        }

        Ok(())
    }

    fn synsent_handler(&self, socket: &mut Socket, packet: &TCPPacket) -> Result<()> {
        dbg!("synsent handler");
        if packet.get_flag() & tcpflags::ACK > 0
            && socket.send_param.unacked_seq <= packet.get_ack()
            && packet.get_ack() <= socket.send_param.next
            && packet.get_flag() & tcpflags::SYN > 0
        {
            socket.recv_param.next = packet.get_seq() + 1;
            socket.recv_param.initial_seq = packet.get_seq();
            socket.send_param.unacked_seq = packet.get_ack();
            socket.send_param.window = packet.get_window_size();

            if socket.send_param.unacked_seq > socket.send_param.initial_seq {
                socket.status = TcpStatus::Established;
                socket.send_tcp_packet(
                    socket.send_param.next,
                    socket.recv_param.next,
                    tcpflags::ACK,
                    &[],
                )?;
                dbg!("status: syssent ->", &socket.status);
                self.publish_event(socket.get_sock_id(), TCPEventKind::ConnectionCompleted);
            } else {
                socket.status = TcpStatus::SynRcvd;
                socket.send_tcp_packet(
                    socket.send_param.next,
                    socket.recv_param.next,
                    tcpflags::ACK,
                    &[],
                )?;

                dbg!("status: synsent ->", &socket.status);
            }
        }

        Ok(())
    }

    fn established_handler(&self, socket: &mut Socket, packet: &TCPPacket) -> Result<()> {
        dbg!("established handler");
        dbg!(
            "received seq",
            socket.send_param.unacked_seq,
            packet.get_ack()
        );
        if socket.send_param.unacked_seq < packet.get_ack()
            && packet.get_ack() <= socket.send_param.next
        {
            socket.send_param.unacked_seq = packet.get_ack();
            self.delete_acked_segment_from_retransmission_queue(socket);
        } else if socket.send_param.next < packet.get_ack() {
            // 未送信セグメントに対するACKは破棄
            dbg!("discard packet", socket.send_param.next, packet.get_ack());
            return Ok(());
        }

        if packet.get_flag() & tcpflags::ACK == 0 {
            // ACKが立っていない受信パケットは破棄
            return Ok(());
        }

        if socket.send_param.window != packet.get_window_size() {
            dbg!("resize window size", packet.get_window_size());

            if packet.get_window_size() == 0 && socket.last_time_window_probe.is_none() {
                dbg!("transit into window probe mode");
                socket.last_time_window_probe = Some(SystemTime::now());
            } else if packet.get_window_size() > 0 && socket.last_time_window_probe.is_some() {
                dbg!("transit into normal mode");
                socket.last_time_window_probe = None;
            }
        }

        socket.send_param.window = packet.get_window_size();

        // RTO計算のために送信済みのパケットに対するACKパケットが返ってきたときに
        // ターンアラウンドタイムを取得する
        // 送信したパケットに対して予想されるACKの値が返ってきたもののみ計算対象にする

        if let Some((idx, _)) = socket
            .sent_times
            .iter()
            .enumerate()
            .find(|(_, times)| times.expected_ack == packet.get_ack())
        {
            let sent_time = socket.sent_times.remove(idx).unwrap();
            let rtt = sent_time.sent_time.elapsed().unwrap();
            socket.rto.next(rtt);

            dbg!(sent_time.sent_time.elapsed().unwrap());
            dbg!(socket.rto.get());
        }

        if !packet.payload().is_empty() {
            self.process_payload(socket, &packet)?;
        }

        if packet.get_flag() & tcpflags::FIN > 0 {
            socket.recv_param.next = packet.get_seq() + 1;
            socket.send_tcp_packet(
                socket.send_param.next,
                socket.recv_param.next,
                tcpflags::ACK,
                &[],
            )?;
            socket.status = TcpStatus::CloseWait;
            self.publish_event(socket.get_sock_id(), TCPEventKind::DataArrived);
        }

        Ok(())
    }

    fn finwait_handler(&self, socket: &mut Socket, packet: &TCPPacket) -> Result<()> {
        dbg!("finwait handler");

        if socket.send_param.unacked_seq < packet.get_ack()
            && packet.get_ack() <= socket.send_param.next
        {
            socket.send_param.unacked_seq = packet.get_ack();
            self.delete_acked_segment_from_retransmission_queue(socket);
        } else if socket.send_param.next < packet.get_ack() {
            return Ok(());
        }

        if packet.get_flag() & tcpflags::ACK == 0 {
            return Ok(());
        }

        if !packet.payload().is_empty() {
            self.process_payload(socket, &packet)?;
        }

        if socket.status == TcpStatus::FinWait1
            && socket.send_param.next == socket.send_param.unacked_seq
        {
            socket.status = TcpStatus::FinWait2;
            dbg!("status: finwait1 ->", &socket.status);
        }

        // 本来はFinWait1状態のときにFINが来たら
        // CLOSING状態に移行するが今回は簡略化のためなし。
        // FinWait2のときにのみFINが来ることとしている
        if packet.get_flag() & tcpflags::FIN > 0 {
            socket.recv_param.next += 1;
            socket.send_tcp_packet(
                socket.send_param.next,
                socket.recv_param.next,
                tcpflags::ACK,
                &[],
            )?;
            self.publish_event(socket.get_sock_id(), TCPEventKind::ConnectionClosed);
        }

        Ok(())
    }

    fn close_handler(&self, socket: &mut Socket, packet: &TCPPacket) -> Result<()> {
        dbg!("closewait | lastack handler");
        socket.send_param.unacked_seq = packet.get_ack();
        Ok(())
    }

    fn process_payload(&self, socket: &mut Socket, packet: &TCPPacket) -> Result<()> {
        if packet.get_flag() & tcpflags::SYN > 0 && packet.get_flag() & tcpflags::ACK > 0 {
            dbg!(packet.get_data_offset());
            dbg!(packet.payload().len());
            dbg!(packet.get_seq());
            dbg!(socket.recv_param.next);
        }
        // 順序が入れ替わっていたときのためにpacket.get_seq() - socket.recv_param.nextでoffsetを調整する
        let offset = socket.recv_buffer.len() - socket.recv_param.window as usize
            + (packet.get_seq() - socket.recv_param.next) as usize;
        let copy_size = cmp::min(packet.payload().len(), socket.recv_buffer.len() - offset);

        socket.recv_buffer[offset..offset + copy_size]
            .copy_from_slice(&packet.payload()[..copy_size]);
        // すでに順序が入れ替わっている可能性があるため、socket.recv_param.tailのほうが大きいか確認する
        socket.recv_param.tail =
            cmp::max(socket.recv_param.tail, packet.get_seq() + copy_size as u32);

        // パケットの順序が入れ替わっていない場合
        if packet.get_seq() == socket.recv_param.next {
            // 順序が入れ替わっていないので、tailがそのままnextになる
            socket.recv_param.next = socket.recv_param.tail;
            // TODO: socket.recv_param.tail - packet.get_seq()ではなくcopy_sizeでも良いか確認する
            // 上のcmp::maxでtailを求めている部分は順序が入れ替わっていなければ必ずpacket.get_seq() + copy_sizeの値が選択される?
            socket.recv_param.window -= (socket.recv_param.tail - packet.get_seq()) as u16;
        }

        // 受信バッファにコピー成功
        if copy_size > 0 {
            socket.send_tcp_packet(
                socket.send_param.next,
                socket.recv_param.next,
                tcpflags::ACK,
                &[],
            )?;
        } else {
            dbg!("recv buffer overflow");
        }

        self.publish_event(socket.get_sock_id(), TCPEventKind::DataArrived);

        Ok(())
    }

    fn delete_acked_segment_from_retransmission_queue(&self, socket: &mut Socket) {
        dbg!("ack accept", socket.send_param.unacked_seq);

        while let Some(item) = socket.retransmission_queue.pop_front() {
            if socket.send_param.unacked_seq > item.packet.get_seq() {
                dbg!("successfully acked", item.packet.get_seq());

                self.publish_event(socket.get_sock_id(), TCPEventKind::Acked);
            } else {
                socket.retransmission_queue.push_front(item);
                break;
            }
        }
    }

    fn timer(&self) {
        dbg!("begin timer thread");

        loop {
            let mut table = self.sockets.write().unwrap();
            for (sock_id, socket) in table.iter_mut() {
                if let Some(last_time) = socket.last_time_window_probe {
                    if last_time.elapsed().unwrap() > WINDOW_PROBE_DURATION {
                        // KeepAliveパケットを送信する
                        socket
                            .send_tcp_packet(
                                socket.send_param.next - 1,
                                socket.recv_param.next,
                                tcpflags::ACK,
                                &[],
                            )
                            .expect("failed to send window probe");
                        socket.last_time_window_probe = Some(SystemTime::now());
                    }
                }

                let mut new_retransmission_queue = VecDeque::new();
                while let Some(mut item) = socket.retransmission_queue.pop_front() {
                    if socket.send_param.unacked_seq > item.packet.get_seq() {
                        // ACKをすでに受信済み
                        dbg!("successfully acked", item.packet.get_seq());
                        self.publish_event(*sock_id, TCPEventKind::Acked);

                        if item.packet.get_flag() & tcpflags::FIN > 0
                            && socket.status == TcpStatus::LastAck
                        {
                            self.publish_event(*sock_id, TCPEventKind::ConnectionClosed);
                        }

                        continue;
                    }

                    if item.latest_transmission_time.elapsed().unwrap() < item.rto {
                        new_retransmission_queue.push_back(item);
                        continue;
                    }

                    if item.transmission_count < MAX_TRANSMISSION {
                        dbg!("retransmit");

                        socket
                            .sender
                            .send_to(item.packet.clone(), IpAddr::V4(socket.remote_addr))
                            .context("failed to retransmit")
                            .unwrap();
                        item.transmission_count += 1;
                        if item.packet.get_flag() == tcpflags::SYN {
                            socket.rto.set(Duration::from_secs(3));
                            item.rto = socket.rto.get();
                        } else {
                            item.rto = socket.rto.backoff();
                        }
                        item.latest_transmission_time = SystemTime::now();
                        // 上の処理のように送信時間を見てRTTを超えていなければ再送するかの確認処理を
                        // 中断するという処理をできるようにするため
                        // 再送キューの一番後ろに配置するようにする
                        new_retransmission_queue.push_back(item);
                    } else {
                        // 再送の上限回数に達したので再送を諦める
                        // 本来はメインスレッドへエラーの通知が必要
                        dbg!("reached MAX_TRANSMISSION");
                        if item.packet.get_flag() & tcpflags::FIN > 0
                            && matches!(
                                socket.status,
                                TcpStatus::LastAck | TcpStatus::FinWait1 | TcpStatus::FinWait2
                            )
                        {
                            self.publish_event(*sock_id, TCPEventKind::ConnectionClosed);
                        }
                    }
                }

                socket.retransmission_queue = new_retransmission_queue;
            }

            drop(table);
            thread::sleep(Duration::from_millis(100));
        }
    }

    // 指定したソケットIDに対して指定したイベントが来るまで待機
    fn wait_event(&self, sock_id: SockID, kind: TCPEventKind) {
        let (lock, cvar) = &self.event_condvar;
        let mut event = lock.lock().unwrap();
        loop {
            if let Some(ref e) = *event {
                if e.sock_id == sock_id && e.kind == kind {
                    break;
                }
            }

            event = cvar.wait(event).unwrap();
        }

        dbg!(&event);
        *event = None;
    }

    // 指定のソケットIDに対してイベント発行
    fn publish_event(&self, sock_id: SockID, kind: TCPEventKind) {
        let (lock, cvar) = &self.event_condvar;
        let mut e = lock.lock().unwrap();
        *e = Some(TCPEvent::new(sock_id, kind));
        cvar.notify_all();
    }
}

impl TCPEvent {
    fn new(sock_id: SockID, kind: TCPEventKind) -> Self {
        Self { sock_id, kind }
    }
}

// ipコマンドを使用して自身のipアドレスを取得する。
// そのため、ipコマンドのバージョンによってはうまく動かない？
// TODO:std::netに自身のipアドレスを取得する関数などはない？
fn get_source_addr_to(addr: Ipv4Addr) -> Result<Ipv4Addr> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!("ip route get {} | grep src", addr))
        .output()?;
    let mut output = str::from_utf8(&output.stdout)?
        .trim()
        .split_ascii_whitespace();

    while let Some(s) = output.next() {
        if s == "src" {
            break;
        }
    }

    let ip = output.next().context("failed to get src ip")?;
    dbg!("source addr", ip);

    ip.parse().context("failed to parse source ip")
}
