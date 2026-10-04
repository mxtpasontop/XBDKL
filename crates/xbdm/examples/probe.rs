//! Sends raw commands given on the command line and prints the replies.
//! Usage: cargo run --example probe -- <ip> '<command>'...
fn main() -> xbdm::Result<()> {
    let mut args = std::env::args().skip(1);
    let addr = args.next().expect("usage: probe <ip> <command>...");
    let mut xbox = xbdm::Client::connect((addr.as_str(), xbdm::XBDM_PORT))?;
    for cmd in args {
        println!("> {cmd}");
        match xbox.command_lines(&cmd) {
            Ok(lines) => lines.iter().for_each(|l| println!("    {l}")),
            Err(e) => println!("< {e}"),
        }
    }
    Ok(())
}
