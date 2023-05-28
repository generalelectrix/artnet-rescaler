use anyhow::{bail, Result};
use artnet_protocol::*;
use std::{
    env::args,
    io,
    net::{Ipv4Addr, ToSocketAddrs, UdpSocket},
    time::{Duration, Instant},
};

const POLL_TIME: Duration = Duration::from_secs(5);
const PORT: u16 = 6454;

fn main() -> Result<()> {
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
                let mut name = <[u8; 18]>::default();
                name[..5].copy_from_slice("hello".as_bytes());
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
                let bytes = resp.write_to_buffer().unwrap();
                socket.send_to(&bytes, addr).unwrap();
            }
        },
        other => {
            panic!("unknown mode {other}");
        }
    }
}
