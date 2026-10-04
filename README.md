# XBDKL: Xbox 360 Neighborhood for Linux

A Linux replacement for **Xbox 360 Neighborhood**, the Windows tool from the Xbox 360 SDK.
It talks to the console's debug monitor (XBDM), so you can browse and transfer files,
take screenshots, see what is running, and debug titles. It is written in Rust and does
not use any Microsoft code.

It works with development kits and with modified consoles (RGH/JTAG, BadUpdate) that load
`xbdm.xex`.

| | |
|---|---|
| **`xbdk`** | Command-line tool: files, screenshots, title info, debugger, and a FUSE mount |
| **`xbdk-gui`** | Desktop app with Console, Files, Screen and Debug tabs |
| **`xbdm`** | The protocol library both are built on |

## Features

- **Discovery:** finds consoles on the local network, or connects by IP or debug name.
- **Files:** list, download, upload (including whole folders), rename, delete, mkdir, and free space per drive.
- **Mount:** the console's drives as a normal folder (FUSE), usable from any file manager, `cp` or `rsync`.
- **Screenshots:** the GPU's tiled framebuffer decoded to PNG; the app also has a live view.
- **Title info:** name, title ID, media ID and version of the running game, including installed title updates.
- **Power:** launch an `.xex`, warm or cold reboot.
- **Debugging:** threads, registers, memory read/write/map, module sections, stop/go, breakpoints, watchpoints, stepping, and a live event stream (debug output, breakpoints, exceptions, module and thread events).

## Install

You need Rust 1.88 or later and, for `xbdk mount`, FUSE 3 (`fuse3` on Arch, `libfuse3-dev` + `fuse3` on Debian/Ubuntu).

```sh
git clone https://github.com/mxtpasontop/XBDKL
cd XBDKL
cargo build --release
# binaries: target/release/xbdk and target/release/xbdk-gui
```

## Quick start

```sh
export XBDK_CONSOLE=192.168.1.50     # IP or debug name; omit to use the only console found

xbdk discover                        # consoles on the network
xbdk info                            # name, kit type, kernel and XDK versions
xbdk title                           # what is running
xbdk screenshot                      # saves screenshot-<date>-<time>.png
xbdk-gui                             # the desktop app
```

## Command line

```sh
# Files (console paths accept / or \; quote paths that use \)
xbdk drives
xbdk df
xbdk ls -l HDD:/Content
xbdk get HDD:/name.txt               # into the current directory
xbdk get -r HDD:/Games/Foo ~/backup  # a whole folder
xbdk put game.xex HDD:/Games/        # a trailing / keeps the local name
xbdk put -r ./Foo HDD:/Games/
xbdk mkdir HDD:/Games
xbdk mv HDD:/a.xex HDD:/b.xex
xbdk rm HDD:/b.xex                   # rm --dir for an empty folder

# Titles and power
xbdk launch HDD:/Games/Foo/default.xex
xbdk reboot [--cold]

# Mount (Ctrl-C or `fusermount3 -u ~/xbox` to unmount)
xbdk mount ~/xbox [--read-only]
```

### Mount notes

XBDM can only transfer whole files. An open file is therefore downloaded completely the
first time it is read or written, and uploaded again when it is closed after a change.
That works well for saves, configs and executables. Opening a multi-gigabyte file means
waiting for the whole download first.

## Desktop app

```sh
xbdk-gui                    # or: xbdk-gui --tab screen
```

- **Console:** system info, the running title, drive usage, reboot and launch.
- **Files:** click drives and folders to browse; right-click to download, launch, rename or delete; drop files or folders on the window to upload them into the current folder.
- **Screen:** single or live screenshots, which can be saved as PNG.
- **Debug:** threads, registers, stop/go/step, breakpoints, a memory viewer and editor, and the event log.

## Debugging

```sh
xbdk modules [--sections default.xex]
xbdk mem read 82000000 100           # hex dump (address and length in hex)
xbdk mem write 82000000 60000000
xbdk mem map
xbdk debug threads
xbdk debug regs THREAD [--fp]
xbdk debug setreg THREAD Gpr3 0
xbdk debug events                    # stream until Ctrl-C
xbdk debug stop | go
xbdk debug halt THREAD | continue THREAD [--exception]
xbdk debug break ADDR [--clear] | --clear-all
xbdk debug watch read|write|execute ADDR [SIZE] [--clear]
xbdk debug step THREAD [--over]
```

Commands that control execution first register this machine as the console's debugger
(`debugger connect`).

> **Warning:** breakpoints patch the title's code. In testing, a breakpoint on a function
> run by dozens of threads, set without a registered debugger, was reported but not
> honoured. Some threads took the trap as a real exception and the game crashed. Use
> breakpoints in code that a single thread runs, and expect a title to crash occasionally.

XBDM has no single-step command. `step` decodes the PowerPC instruction, places temporary
breakpoints on each possible next address, and resumes. Other threads keep running, so
stepping through code that other threads also execute is unreliable.

**Hardware status:** threads, registers of running threads, modules, memory read/write/map,
events, stop and go have been tested on a console. Breakpoints with a registered debugger,
stepping, watchpoints, halt/continue and setreg have not been verified yet.

## As a library

```rust
use xbdm::Client;

fn main() -> xbdm::Result<()> {
    let mut xbox = Client::connect(("192.168.1.50", xbdm::XBDM_PORT))?;
    let title = xbdm::title::running_title(&mut xbox)?;
    println!("running: {}", title.name.unwrap_or(title.executable));
    for entry in xbox.list_dir("HDD:\\")? {
        println!("{} {}", entry.name, entry.size);
    }
    let shot = xbox.screenshot()?; // 8-bit RGB in shot.rgb
    println!("{}x{}", shot.width, shot.height);
    Ok(())
}
```

## Protocol notes

Observations from a retail console running BadUpdate on kernel 17559 with a 2012 `xbdm.xex`.
Other XBDM builds may behave differently.

- TCP and UDP port 730. Commands are text lines; replies start with a status such as `200- OK`, `202-` (lines until `.`), `203-` (binary data) or `204-` (send binary data).
- `getfile` replies with a 4-byte little-endian length, then the data. `offset`/`size` are ignored, and `writefile`/`fileeof` are unknown, so files can only be transferred whole.
- `sendfile` with `length=0x0` creates the file and then waits for data forever. The client opens a new connection after an empty upload.
- A path that does not exist returns `414 access denied`, not `402 file not found`.
- The broadcast discovery reply carries an empty name, so names are fetched with `dbgname`.
- Screenshots are 32-bit B,G,R,X pixels in the Xenos tiled layout. The `format` field's bit `0x100` marks a tiled buffer.
- The running title's execution info, which includes the version from an installed title update, is read from its in-memory XEX header, found through the kernel export `XexExecutableModuleHandle`.
- `notify` requires `reconnectport=…`. With `reverse`, the same connection turns into the event stream.
- `stop` halts execution, but `isstopped` still reports threads as running and volatile registers stay unreadable. Only threads stopped by a debug event appear as stopped.

## Development

```sh
cargo test                                             # unit tests and a mock console
cargo run --example probe -- 192.168.1.50 'threads'    # send raw XBDM commands
```

The mock-console tests replay responses captured from real hardware, so they run without
a console.

## Disclaimer

This project is not affiliated with or endorsed by Microsoft. "Xbox" and "Xbox 360" are
trademarks of Microsoft. It contains no Microsoft code or SDK files; it is a clean
implementation of the network protocol for interoperability. Use it only on consoles you
own, and expect debugging features to crash titles occasionally.

## License

[MIT](LICENSE)
