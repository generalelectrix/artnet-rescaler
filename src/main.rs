use anyhow::{anyhow, bail, Result};
use artnet_protocol::*;
use log::{error, info, warn};
use midir::{MidiIO, MidiInput, MidiInputConnection};
use number::UnipolarFloat;
use simplelog::{Config as LogConfig, SimpleLogger};
use std::{
    env::args,
    io,
    net::{Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket},
    sync::mpsc::{channel, Sender},
    thread,
    time::{Duration, Instant},
};

const POLL_TIME: Duration = Duration::from_secs(5);
const PORT: u16 = 6454;

fn main() -> Result<()> {
    SimpleLogger::init(log::LevelFilter::Info, LogConfig::default())?;
    match args().nth(1).unwrap().as_str() {
        "poll" => {
            let socket = UdpSocket::bind(("0.0.0.0", PORT)).unwrap();
            let broadcast_addr = ("255.255.255.255", PORT).to_socket_addrs()?.next().unwrap();
            socket.set_broadcast(true).unwrap();
            let buff = ArtCommand::Poll(Poll::default()).write_to_buffer()?;
            socket.send_to(&buff, broadcast_addr).unwrap();

            println!("Polling for devices...");
            let start = Instant::now();
            socket.set_read_timeout(Some(Duration::from_secs(1)))?;
            loop {
                if start.elapsed() > POLL_TIME {
                    return Ok(());
                }

                let mut buffer = [0u8; 1024];
                let (length, _addr) = match socket.recv_from(&mut buffer) {
                    Err(err) => {
                        if err.kind() == io::ErrorKind::WouldBlock {
                            continue;
                        }
                        bail!(err);
                    }
                    Ok(md) => md,
                };
                let command = ArtCommand::from_buffer(&buffer[..length])?;

                if let ArtCommand::PollReply(reply) = command {
                    println!(
                        "Device found at {}: {}",
                        reply.address,
                        String::from_utf8_lossy(&reply.short_name),
                    );
                }
            }
        }
        "node" => loop {
            let socket = UdpSocket::bind(("192.168.1.127", PORT)).unwrap();
            let mut buffer = [0u8; 1024];
            let (length, addr) = socket.recv_from(&mut buffer)?;
            let command = ArtCommand::from_buffer(&buffer[..length])?;

            if let ArtCommand::Poll(_) = command {
                let poll_resp = poll_response().unwrap();
                socket.send_to(&poll_resp, addr).unwrap();
            }
        },
        "rescale" => {
            let socket = UdpSocket::bind(("0.0.0.0", PORT)).unwrap();
            let target = ("10.10.0.4", PORT)
                .to_socket_addrs()
                .unwrap()
                .next()
                .unwrap();
            run_rescale(socket, &[target])?;
            Ok(())
        }
        other => {
            panic!("unknown mode {other}");
        }
    }
}

fn run_rescale(socket: UdpSocket, destinations: &[SocketAddr]) -> Result<()> {
    let (send, recv) = channel::<Action>();

    let mut scale = UnipolarFloat::ONE;

    let artnet_send = send.clone();
    let recv_socket = socket.try_clone().unwrap();
    thread::spawn(move || {
        let mut buffer = [0u8; 1024];
        loop {
            match receive_artnet(&recv_socket, &mut buffer) {
                Ok(Some(action)) => artnet_send.send(action).unwrap(),
                Ok(None) => (),
                Err(err) => {
                    error!("artnet receive error: {err}");
                }
            }
        }
    });

    let _input = Input::new("MC-8".to_string(), send)?;

    loop {
        let action = recv.recv().unwrap();
        match action {
            Action::PollResp(addr) => {
                let poll_resp = match poll_response() {
                    Ok(msg) => msg,
                    Err(err) => {
                        error!("failed to create poll respose: {err}");
                        continue;
                    }
                };
                if let Err(err) = socket.send_to(&poll_resp, addr) {
                    error!("artnet poll response send error: {err}");
                }
            }
            Action::Scale(val) => {
                scale = val;
            }
            Action::Packet(mut output) => {
                for val in output.data.as_mut() {
                    *val = ((*val as f64) * scale.val()) as u8;
                }
                let command = ArtCommand::Output(output);
                let buffer = match command.write_to_buffer() {
                    Ok(buf) => buf,
                    Err(err) => {
                        error!("artnet serialization error: {err}");
                        continue;
                    }
                };
                for dest in destinations {
                    if let Err(err) = socket.send_to(&buffer, dest) {
                        error!("artnet send error: {err}");
                    }
                }
            }
        }
    }
}

pub enum Action {
    Scale(UnipolarFloat),
    Packet(Output),
    PollResp(SocketAddr),
}

fn receive_artnet(socket: &UdpSocket, buffer: &mut [u8]) -> Result<Option<Action>> {
    let (length, mut addr) = socket.recv_from(buffer)?;
    let command = ArtCommand::from_buffer(&buffer[..length])?;
    match command {
        ArtCommand::Poll(_) => {
            info!("poll from {addr}");
            addr.set_port(PORT);
            Ok(Some(Action::PollResp(addr)))
        }
        ArtCommand::Output(output) => Ok(Some(Action::Packet(output))),
        _ => Ok(None),
    }
}

pub struct Input {
    _conn: MidiInputConnection<()>,
}

impl Input {
    pub fn new(name: String, sender: Sender<Action>) -> Result<Self> {
        let input = MidiInput::new("tunnels")?;
        let port = get_named_port(&input, &name)?;
        let handler_name = name.clone();

        let conn = input
            .connect(
                &port,
                &name,
                move |_, msg: &[u8], _| {
                    let event_type = match msg[0] >> 4 {
                        8 => EventType::NoteOff,
                        9 => EventType::NoteOn,
                        11 => EventType::ControlChange,
                        other => {
                            warn!(
                                "Ignoring midi input event on {} of unimplemented type {}.",
                                handler_name, other
                            );
                            return;
                        }
                    };
                    if event_type != EventType::ControlChange {
                        return;
                    }
                    let channel = msg[0] & 15;
                    if channel != 0 {
                        return;
                    }
                    sender
                        .send(Action::Scale(unipolar_from_midi(msg[2])))
                        .unwrap();
                },
                (),
            )
            .map_err(|err| anyhow!("failed to connect to midi input: {err}"))?;
        Ok(Input { _conn: conn })
    }
}

fn get_named_port<T: MidiIO>(source: &T, name: &str) -> Result<T::Port> {
    for port in source.ports() {
        if let Ok(this_name) = source.port_name(&port) {
            if this_name == name {
                return Ok(port);
            }
        }
    }
    bail!("no port found with name {}", name);
}

fn unipolar_from_midi(val: u8) -> UnipolarFloat {
    UnipolarFloat::new(val as f64 / 127.)
}

/// Specification for what type of midi event.
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum EventType {
    NoteOn,
    NoteOff,
    ControlChange,
}

fn poll_response() -> Result<Vec<u8>> {
    let mut name = <[u8; 18]>::default();
    name[..8].copy_from_slice("rescaler".as_bytes());
    let resp = ArtCommand::PollReply(Box::new(PollReply {
        address: Ipv4Addr::new(1, 1, 1, 1),
        port: PORT,
        version: Default::default(),
        port_address: Default::default(),
        oem: Default::default(),
        ubea_version: Default::default(),
        status_1: 0,
        esta_code: 0,
        short_name: name,
        long_name: [0; 64],
        node_report: [0; 64],
        num_ports: Default::default(),
        port_types: Default::default(),
        good_input: Default::default(),
        good_output: Default::default(),
        swin: Default::default(),
        swout: Default::default(),
        sw_video: Default::default(),
        sw_macro: Default::default(),
        sw_remote: Default::default(),
        spare: Default::default(),
        style: Default::default(),
        mac: Default::default(),
        bind_ip: Default::default(),
        bind_index: Default::default(),
        status_2: Default::default(),
        filler: Default::default(),
    }));
    Ok(resp.write_to_buffer()?)
}
