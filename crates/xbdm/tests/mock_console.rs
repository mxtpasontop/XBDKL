//! Exercises the client against a scripted fake console on localhost.
//! Responses mirror what a real console (kernel 17559 + xbdm.xex) sent.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use xbdm::Client;

/// Starts a fake console that serves one connection with `handler`.
fn mock(handler: impl FnOnce(&mut BufReader<TcpStream>, &mut TcpStream) + Send + 'static) -> Client {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        writer.write_all(b"201- connected\r\n").unwrap();
        handler(&mut reader, &mut writer);
    });
    Client::connect(addr).unwrap()
}

fn expect_line(reader: &mut BufReader<TcpStream>, expected: &str) {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line, format!("{expected}\r\n"));
}

#[test]
fn info_and_listing() {
    let mut client = mock(|r, w| {
        expect_line(r, "dbgname");
        w.write_all(b"200- Jtag\r\n").unwrap();
        expect_line(r, "drivelist");
        w.write_all(b"202- multiline response follows\r\ndrivename=\"HDD\"\r\ndrivename=\"USB0\"\r\n.\r\n")
            .unwrap();
        expect_line(r, r#"dirlist name="HDD:\""#);
        w.write_all(
            b"202- multiline response follows\r\n\
              name=\"name.txt\" sizehi=0x0 sizelo=0x16 createhi=0x01c5ef5c createlo=0x81954400 changehi=0x01c5ef5c changelo=0x81954400\r\n\
              name=\"Cache\" sizehi=0x0 sizelo=0x0 createhi=0x01c5ef5c createlo=0x81954400 changehi=0x01c5ef5c changelo=0x81954400 directory\r\n\
              .\r\n",
        )
        .unwrap();
        expect_line(r, r#"dirlist name="HDD:\missing""#);
        w.write_all(b"402- file not found\r\n").unwrap();
    });

    assert_eq!(client.debug_name().unwrap(), "Jtag");
    assert_eq!(client.drives().unwrap(), ["HDD", "USB0"]);
    let entries = client.list_dir("HDD:").unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!((entries[0].name.as_str(), entries[0].size, entries[0].is_dir), ("name.txt", 22, false));
    assert!(entries[1].is_dir);
    assert!(client.list_dir("HDD:/missing").unwrap_err().is_not_found());
}

#[test]
fn file_transfer() {
    let mut client = mock(|r, w| {
        expect_line(r, r#"getfile name="HDD:\name.txt""#);
        w.write_all(b"203- binary response follows\r\n\x05\x00\x00\x00hello").unwrap();

        expect_line(r, r#"sendfile name="HDD:\up.bin" length=0x3"#);
        w.write_all(b"204- send binary data\r\n").unwrap();
        let mut body = [0u8; 3];
        r.read_exact(&mut body).unwrap();
        assert_eq!(&body, b"abc");
        w.write_all(b"200- OK\r\n").unwrap();
    });

    let mut out = Vec::new();
    assert_eq!(client.get_file("HDD:\\name.txt", &mut out).unwrap(), 5);
    assert_eq!(out, b"hello");
    client.send_file("HDD:/up.bin", &mut &b"abc"[..], 3).unwrap();
}

#[test]
fn rejects_injection_without_sending() {
    let mut client = mock(|r, w| {
        // Only the follow-up command should arrive.
        expect_line(r, "dbgname");
        w.write_all(b"200- Jtag\r\n").unwrap();
    });
    assert!(client.list_dir("HDD:\\\"\r\nmagicboot cold").is_err());
    assert_eq!(client.debug_name().unwrap(), "Jtag");
}

#[test]
fn empty_upload_replaces_stuck_connection() {
    // Real xbdm answers a zero-length sendfile with 204 and then never replies again.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        // The first connection stays open (and silent) until the thread ends, like the console.
        let (first, _) = listener.accept().unwrap();
        let mut stuck_w = first.try_clone().unwrap();
        let mut stuck_r = BufReader::new(first);
        stuck_w.write_all(b"201- connected\r\n").unwrap();
        expect_line(&mut stuck_r, r#"sendfile name="HDD:\empty.txt" length=0x0"#);
        stuck_w.write_all(b"204- send binary data\r\n").unwrap();

        let (second, _) = listener.accept().unwrap();
        let mut w = second.try_clone().unwrap();
        let mut r = BufReader::new(second);
        w.write_all(b"201- connected\r\n").unwrap();
        expect_line(&mut r, "dbgname");
        w.write_all(b"200- Jtag\r\n").unwrap();
    });

    let mut client = Client::connect(addr).unwrap();
    client.send_file("HDD:\\empty.txt", &mut std::io::empty(), 0).unwrap();
    assert_eq!(client.debug_name().unwrap(), "Jtag");
}
