use anyhow::Result;
use std::{env, fs, net::Ipv4Addr, str};
use toytcp::tcp::TCP;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let addr: Ipv4Addr = args[1].parse()?;
    let port: u16 = args[2].parse()?;
    let filepath: &str = &args[3];

    file_server(addr, port, filepath)?;

    Ok(())
}

fn file_server(addr: Ipv4Addr, port: u16, filepath: &str) -> Result<()> {
    let tcp = TCP::new();
    let sock_id = tcp.listen(addr, port)?;
    loop {
        let sock_id = tcp.accept(sock_id)?;
        let cloned_tcp = tcp.clone();
        let mut v = Vec::new();
        loop {
            let mut buffer = [0; 1024];
            let nbytes = cloned_tcp.recv(sock_id, &mut buffer).unwrap();
            if nbytes == 0 {
                dbg!("closing connection...");
                tcp.close(sock_id)?;
                break;
            }
            v.extend_from_slice(&buffer[..nbytes]);
        }
        fs::write(filepath, &v).unwrap();
    }
}