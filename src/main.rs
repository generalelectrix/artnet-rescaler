use anyhow::{anyhow, bail, Result};
use artnet_protocol::*;
use log::{debug, error, info, warn};
use midir::{MidiIO, MidiInput, MidiInputConnection};
use number::UnipolarFloat;
use rosc::{encoder, OscMessage, OscPacket, OscType};
use serde::Deserialize;
use simplelog::{Config as LogConfig, SimpleLogger};
use std::{
    collections::HashMap,
    env::args,
    fs::File,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    path::Path,
    sync::mpsc::{channel, Sender},
    thread,
};

const PORT: u16 = 6454;

fn main() -> Result<()> {
    let config_path = args().nth(1).unwrap();
    let config_file = File::open(Path::new(&config_path))?;
    let config: Config = serde_yaml::from_reader(&config_file).unwrap();

    SimpleLogger::init(log::LevelFilter::Info, LogConfig::default())?;
    let socket = UdpSocket::bind(("0.0.0.0", PORT)).unwrap();
    run_rescale(socket, config)
}

fn run_rescale(socket: UdpSocket, config: Config) -> Result<()> {
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

    let send_osc = forward_osc(&config)?;

    let input = Input::new(
        config.midi_port.clone(),
        config.rescale_midi_control,
        send,
        send_osc,
    );
    if let Err(err) = &input {
        error!("failed to open midi port: {err}");
    }

    let actions = config.actions();

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
                let Some(action) = actions.get(&output.port_address) else {
                    debug!("Ignoring non-configured universe {:?}", output.port_address);
                    continue;
                };
                if action.rescale {
                    rescale_universe(scale, &mut output);
                }
                if !action.remap.is_empty() {
                    remap_universe(&action.remap, &mut output);
                }
                let command = ArtCommand::Output(output);
                let buffer = match command.write_to_buffer() {
                    Ok(buf) => buf,
                    Err(err) => {
                        error!("artnet serialization error: {err}");
                        continue;
                    }
                };
                let dest = SocketAddrV4::new(action.destination, PORT);
                if let Err(err) = socket.send_to(&buffer, dest) {
                    error!("artnet send error: {err}");
                }
            }
        }
    }
}

fn rescale_universe(scale: UnipolarFloat, output: &mut Output) {
    for val in output.data.as_mut() {
        *val = ((*val as f64) * scale.val()) as u8;
    }
}

fn remap_universe(remappings: &[Remapping], output: &mut Output) {
    let mut buffer = vec![0u8; 512];
    let input = output.data.as_ref();
    for remap in remappings {
        let Some(vals) = input.get(remap.start..remap.start+remap.length) else {
            warn!("remapping out of range for {:?} (input length {}): {:?}", output.port_address, input.len(), remap);
            continue;
        };
        buffer[remap.new_start..remap.new_start + remap.length].copy_from_slice(vals);
    }
    output.data = buffer.into()
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
    pub fn new(
        name: String,
        rescale_control: MidiControl,
        sender: Sender<Action>,
        osc_forward: Sender<(MidiControl, u8)>,
    ) -> Result<Self> {
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
                    let control = MidiControl {
                        channel: msg[0] & 15,
                        control: msg[1],
                    };
                    let val = msg[2];
                    // If this message matches scaler config, use it.
                    if control == rescale_control {
                        sender.send(Action::Scale(unipolar_from_midi(val))).unwrap();
                        return;
                    }
                    // Otherwise, pass the message on for OSC forwarding.
                    osc_forward.send((control, val)).unwrap();
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

#[derive(Deserialize)]
pub struct Config {
    midi_port: String,
    rescale_midi_control: MidiControl,
    osc_forward: OscForward,
    universes: HashMap<u8, UniverseActions>,
}

impl Config {
    fn actions(&self) -> HashMap<PortAddress, UniverseActions> {
        self.universes
            .iter()
            .map(|(id, actions)| ((*id).into(), actions.clone()))
            .collect()
    }
}

#[derive(Deserialize, Clone)]
pub struct UniverseActions {
    #[serde(default)]
    rescale: bool,
    #[serde(default)]
    remap: Vec<Remapping>,
    destination: Ipv4Addr,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Remapping {
    start: usize,
    length: usize,
    new_start: usize,
}

#[derive(Deserialize, PartialEq, Eq, Debug, Clone, Copy, Hash)]
pub struct MidiControl {
    channel: u8,
    control: u8,
}

#[derive(Deserialize, PartialEq, Eq, Debug, Clone)]
pub struct OscMapping {
    midi: MidiControl,
    osc: String,
}

#[derive(Deserialize, PartialEq, Eq, Debug, Clone)]
pub struct OscForward {
    destination: SocketAddr,
    mappings: Vec<OscMapping>,
}

fn forward_osc(config: &Config) -> Result<Sender<(MidiControl, u8)>> {
    let (send, recv) = channel();

    let dest = config.osc_forward.destination;
    let mapping: HashMap<_, _> = config
        .osc_forward
        .mappings
        .iter()
        .map(|mapping| (mapping.midi, mapping.osc.clone()))
        .collect();

    let socket = UdpSocket::bind("0.0.0.0:0")?;

    thread::spawn(move || loop {
        let (midi_control, val) = recv.recv().unwrap();
        let Some(osc_control) = mapping.get(&midi_control) else {
                warn!("ignoring unmapped midi mapping {:?}", midi_control);
                continue;
            };
        let payload = unipolar_from_midi(val);
        let osc_msg = OscMessage {
            addr: osc_control.clone(),
            args: vec![OscType::Double(payload.val())],
        };

        let packet = OscPacket::Message(osc_msg);
        let msg_buf = match encoder::encode(&packet) {
            Ok(buf) => buf,
            Err(err) => {
                error!("Error encoding OSC packet {packet:?}: {err}.");
                continue;
            }
        };
        if let Err(err) = socket.send_to(&msg_buf, dest) {
            error!("OSC send error to address {dest}: {}.", err);
        }
    });

    Ok(send)
}
