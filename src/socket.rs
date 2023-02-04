use crate::packet::TCPPacket;
use crate::tcpflags;
use anyhow::{Context, Result};
use pnet::packet::{ip::IpNextHeaderProtocols, Packet};
use pnet::transport::{self, TransportChannelType, TransportProtocol, TransportSender};
use pnet::util;
use std::collections::VecDeque;
use std::fmt::{self, Display};
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::time::SystemTime;

const SOCKET_BUFFER_SIZE: usize = 4380;

#[derive(Debug, Hash, Eq, PartialEq, Clone, Copy)]
pub struct SockID(pub Ipv4Addr, pub Ipv4Addr, pub u16, pub u16);

pub struct Socket {
    pub local_addr: Ipv4Addr,
    pub remote_addr: Ipv4Addr,
    pub local_port: u16,
    pub remote_port: u16,

    pub send_param: SendParam,
    pub recv_param: RecvParam,
    pub status: TcpStatus,

    pub sender: TransportSender,
    pub connected_connection_queue: VecDeque<SockID>, // 接続済みソケットを保持するキュー、リスニングソケットのみ使用
    pub listening_socket: Option<SockID>, // 生成元のリスニングソケット、接続済みソケットのみ使用
}

#[derive(Clone, Debug)]
pub struct SendParam {
    pub unacked_seq: u32,
    pub next: u32,
    pub window: u16,
    pub initial_seq: u32,
}

#[derive(Clone, Debug)]
pub struct RecvParam {
    pub next: u32,
    pub window: u16,
    pub initial_seq: u32,
    pub tail: u32,
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub enum TcpStatus {
    Listen,
    SynSent,
    SynRcvd,
    Established,
    FinWait1,
    FinWait2,
    TimeWait,
    CloseWait,
    LastAck,
}

impl Socket {
    pub fn new(
        local_addr: Ipv4Addr,
        remote_addr: Ipv4Addr,
        local_port: u16,
        remote_port: u16,
        status: TcpStatus,
    ) -> Result<Self> {
        // トランスポート層のTCPに投げるように設定？
        let (sender, _) =  transport::transport_channel(
            65535,
            TransportChannelType::Layer4(TransportProtocol::Ipv4(IpNextHeaderProtocols::Tcp))
        )?;

        let send_param = SendParam {
            unacked_seq: 0,
            initial_seq: 0,
            next: 0,
            window: SOCKET_BUFFER_SIZE as u16,
        };

        let recv_param = RecvParam {
            initial_seq: 0,
            next: 0,
            window: SOCKET_BUFFER_SIZE as u16,
            tail: 0
        };

        let connected_connection_queue = VecDeque::new();
        let listening_socket = None;

        Ok(Self {
            local_addr,
            remote_addr,
            local_port,
            remote_port,

            send_param,
            recv_param,
            status,

            sender,
            connected_connection_queue,
            listening_socket,
        })
    }

    pub fn send_tcp_packet(
        &mut self,
        seq: u32,
        ack: u32,
        flag: u8,
        payload: &[u8]
    ) -> Result<usize> {
        let mut tcp_packet = TCPPacket::new(payload.len());
        tcp_packet.set_src(self.local_port);
        tcp_packet.set_dst(self.remote_port);
        tcp_packet.set_seq(seq);
        tcp_packet.set_ack(ack);
        tcp_packet.set_data_offset(5);
        tcp_packet.set_flag(flag);
        tcp_packet.set_window_size(self.recv_param.window);
        tcp_packet.set_payload(payload);
        tcp_packet.set_checksum(util::ipv4_checksum(&tcp_packet.packet(), 8, &[], &self.local_addr, &self.remote_addr, IpNextHeaderProtocols::Tcp));

        // tcp_packet.clone()のcloneは必要？
        let sent_size = self.sender.send_to(tcp_packet.clone(), IpAddr::V4(self.remote_addr)).context(format!("failed to send: \n{:?}", tcp_packet))?;

        dbg!("sent", &tcp_packet);

        Ok(sent_size)
    }

    pub fn get_sock_id(&self) -> SockID {
        SockID(
            self.local_addr,
            self.remote_addr,
            self.local_port,
            self.remote_port,
        )
    }
}

impl Display for TcpStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            TcpStatus::Listen => "LISTEN",
            TcpStatus::SynSent => "SYNSENT",
            TcpStatus::SynRcvd => "SYNRCVD",
            TcpStatus::Established => "ESTABLISHED",
            TcpStatus::FinWait1 => "FINWAIT1",
            TcpStatus::FinWait2 => "FINWAIT2",
            TcpStatus::TimeWait => "TIMEWAIT",
            TcpStatus::CloseWait => "CLOSEWAIT",
            TcpStatus::LastAck => "LASTACK",
        };

        write!(f, "{}", msg)
    }
}