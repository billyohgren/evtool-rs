# evtool-rs

A command-line tool for Evox M8/M8+ Xbox BIOS images: unpack the kernel, apply an IPS patch, and pack it back. Its output is compatible with EVtool 1.0.9's unpack and pack.

This repository contains no BIOS, kernel or patch files. You need to supply your own.

## Download

Prebuilt binaries are on the [Releases](https://github.com/billyohgren/evtool-rs/releases) page:

| System | File |
|---|---|
| Windows | `evtool-rs-<version>-windows-x64.zip` |
| macOS, Apple Silicon (M1 and later) | `evtool-rs-<version>-macos-arm64.tar.gz` |
| macOS, Intel | `evtool-rs-<version>-macos-x64.tar.gz` |
| Linux | `evtool-rs-<version>-linux-x64.tar.gz` |

Unpack the archive and put your BIOS (`M8plus.bin`) and patch (`m8plus_kpatcher.ips`) in the same folder as `evtool-rs`. The binaries are not signed, so Windows and macOS warn the first time you run them.

## Usage

The example unpacks the kernel from `M8plus.bin`, applies `m8plus_kpatcher.ips`, and packs it into a new BIOS, `M8plus_kpatched.bin`.

### Windows

Open the folder in File Explorer, type `cmd` in the address bar and press Enter. Then run:

```bat
evtool-rs.exe unpack M8plus.bin xboxkrnl.img
evtool-rs.exe ips    xboxkrnl.img m8plus_kpatcher.ips patched.img
evtool-rs.exe pack   patched.img M8plus.bin M8plus_kpatched.bin
```

If SmartScreen says "Windows protected your PC", click **More info** and then **Run anyway**.

### macOS and Linux

Open a terminal in the folder and run:

```sh
./evtool-rs unpack M8plus.bin xboxkrnl.img
./evtool-rs ips    xboxkrnl.img m8plus_kpatcher.ips patched.img
./evtool-rs pack   patched.img M8plus.bin M8plus_kpatched.bin
```

On macOS, if the system says the developer cannot be verified, clear the quarantine flag first:

```sh
xattr -d com.apple.quarantine evtool-rs
```

### What the commands do

| Command | Does |
|---|---|
| `unpack <rom.bin> <xboxkrnl.img>` | Extracts the kernel from a BIOS |
| `ips <in> <patch.ips> <out>` | Applies an IPS patch to a file |
| `pack <xboxkrnl.img> <base-rom.bin> <out.bin>` | Packs a kernel into a copy of the base BIOS |

Pack with the base ROM that matches the console: `M8plus.bin` for hardware 1.0–1.5, `M8plus_16.bin` for 1.6. The two differ only in the init table at the start. The kernel is the same in both.

EVtool's patch options (IGR, LBA48, logo, boot flags) are not included.

## Building from source

With [Rust](https://rustup.rs) installed:

```sh
cargo build --release
```

The binary is `target/release/evtool-rs`.

## ROM layout

The ROM is 256 KB. Near its top is a `$EvoxRom$` header (see `src/rom.rs`). From the top down:

1. The Evox loader. Its size is given in the header.
2. A 0x4658 byte block that EVtool rewrites with identical contents. `pack` leaves it as it is.
3. The kernel's `.data`, up to its last non-zero dword. Its size and location are written into the kernel's DOS header at 0x28–0x3b.
4. The compressed kernel. It may grow downward over the zero padding, but not into the init table.

Nothing is encrypted. The kernel is LZX-compressed: 128 KB window, 32 KB frames, each frame prefixed by `u16 compressed, u16 uncompressed`, with E8 translation (size 12000000).

## Compressor

`src/lzx.rs` has its own LZX decoder and encoder. The encoder:

- chooses tokens with a cost-based optimal parse, refined over six passes;
- splits blocks at 512-byte segments, merging neighbours while that saves bits;
- uses aligned-offset blocks where they are smaller.

The ROM has only a few KB to spare, so compression has to match Microsoft's. On the stock M8+ kernel this encoder gives 218,782 bytes, against EVtool's 218,820.

The decoder follows Microsoft's E8 handling. The `lzxd` crate treats an E8 value of exactly 0 differently, so it is only a dev-dependency, used as a cross-check:

```sh
ROM=out.bin IMG=expected.img cargo test --release -- --ignored
```

## License

MIT. See `LICENSE`.

Xbox is a trademark of Microsoft. This project is not affiliated with or endorsed by Microsoft or the Evox team.
