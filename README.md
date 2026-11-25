# wiredata

Tools for testing devices that talk over serial ports and UDP.

- **Talker** sends data to a device. You set up the messages, and Talker sends
  them on a schedule over a serial port or UDP.
- **Listener** receives data from serial ports and UDP. It shows the bytes
  exactly as they arrived and can record them to files. It can watch several
  channels at once, unattended, for weeks.
- **Linemon** watches a single serial or UDP port. Use it when you want a quick
  look at one stream.

Talker and Listener each come in a window version and a command-line version.

## Download

Get the latest version from the
[Releases page](https://github.com/BillFanning/wiredata/releases/latest).

| System | File |
|--------|------|
| Windows 10 or 11 (64-bit) | `wiredata-<version>-windows-x86_64.zip` |
| Linux (64-bit, glibc 2.35 or newer) | `wiredata-<version>-linux-x86_64.tar.gz` |

Unzip the file anywhere. There is nothing to install.

| Program | Opens |
|---------|-------|
| `talker-gui` | Talker in a window |
| `talker` | Talker on the command line |
| `listener-gui` | Listener in a window |
| `listener` | Listener on the command line |
| `linemon` | Linemon in a window |

**Windows:** these programs are not code-signed yet. The first time you run
one, Windows may show "Windows protected your PC". Click **More info**, then
**Run anyway**.

**Linux:** to use a serial port, your user must be in the `dialout` group:
`sudo usermod -aG dialout $USER`. Log out and back in after running it.

To check that a download arrived intact, compare its SHA-256 hash with the one
in `SHA256SUMS.txt` on the release page.

## What Talker sends

- Raw bytes, written as hex
- UTF-8 or ASCII text. ASCII supports the CP437, Windows-1252, Mac OS Roman
  and ISO-8859-1 code pages. Text can contain non-printable bytes.
- UTF-16 text, in either byte order
- NMEA 0183 sentences

Each message can start with a timestamp and end with a checksum (CRC-8,
CRC-16, CRC-32 or XOR). You save a setup as a profile, and the window and
command-line versions share the same profiles.

## What Listener shows

- Raw, Rendered and Hex views of the stream
- Find, highlighting and actions triggered by byte patterns
- Recording of the exact received bytes, or of what the display shows

Listener only receives. It never sends anything to the port.

## Build from source

You need Rust 1.99 or newer ([rustup.rs](https://rustup.rs)).

```sh
cargo build --release -p talker -p listener
```

The programs are written to `target/release/`. On Linux, building also needs
these packages: `pkg-config libudev-dev libxkbcommon-dev libwayland-dev
libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
libgl1-mesa-dev`.

## Documentation

Full specifications are in each program's `docs/` folder:
[Talker](talker/docs/talker_specification.md),
[Listener](listener/docs/listener_specification.md),
[NMEA 0183 library](nmea0183/docs/nmea0183_specification.md).

## License

You may use this project under either of these licenses, whichever you prefer:

- [MIT License](LICENSE-MIT)
- [Apache License 2.0](LICENSE-APACHE)
