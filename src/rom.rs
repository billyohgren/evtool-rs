//! Evox M8 ROM layout, as EVtool 1.0.9 reads and writes it.
//!
//! The top of the ROM holds a `$EvoxRom$` header with the location of the kernel:
//!
//! ```text
//! +0x00  "$EvoxRom$\0"
//! +0x0a  u32  loader size (bytes at the very top of the ROM)
//! +0x0e  u32  compressed kernel address: 0xff800000 - (rom_size - offset)
//! +0x12  u32  compressed kernel size
//! +0x16  u32  uncompressed kernel image size
//! ```
//!
//! Below the loader, growing downwards: a 0x4658 byte block EVtool never changes,
//! the kernel's initialised `.data`, then the compressed kernel. The kernel is a PE
//! image whose `.data` is stored separately; its DOS header (0x28..0x3c) records
//! where that data lives.

use crate::lzx;

const MAGIC: &[u8; 10] = b"$EvoxRom$\0";
const FIXED_BLOCK_SIZE: usize = 0x4658;

pub struct Header {
    pos: usize,
    pub loader_size: u32,
    pub kernel_addr: u32,
    pub compressed_size: u32,
    pub kernel_size: u32,
}

impl Header {
    pub fn find(rom: &[u8]) -> Result<Header, String> {
        // EVtool scans down from the end in 2 byte steps, over the last 4 KB.
        let mut pos = rom.len() & !0xf;
        while pos > rom.len().saturating_sub(0x1000) {
            if rom.get(pos..pos + MAGIC.len()) == Some(MAGIC) {
                return Ok(Header {
                    pos,
                    loader_size: u32_at(rom, pos + 0x0a),
                    kernel_addr: u32_at(rom, pos + 0x0e),
                    compressed_size: u32_at(rom, pos + 0x12),
                    kernel_size: u32_at(rom, pos + 0x16),
                });
            }
            pos -= 2;
        }
        Err("no $EvoxRom$ header; not an Evox M8 ROM".into())
    }

    pub fn kernel_offset(&self, rom_len: usize) -> usize {
        (self
            .kernel_addr
            .wrapping_add(rom_len as u32)
            .wrapping_sub(0x80_0000)
            & 0xff_ffff) as usize
    }

    fn write(&self, rom: &mut [u8]) {
        put_u32(rom, self.pos + 0x0e, self.kernel_addr);
        put_u32(rom, self.pos + 0x12, self.compressed_size);
        put_u32(rom, self.pos + 0x16, self.kernel_size);
    }
}

/// Extracts the kernel image (`xboxkrnl.img`) with its `.data` merged back in.
pub fn unpack(rom: &[u8]) -> Result<Vec<u8>, String> {
    let h = Header::find(rom)?;
    let off = h.kernel_offset(rom.len());
    let compressed = rom
        .get(off..off + h.compressed_size as usize)
        .ok_or("compressed kernel lies outside the ROM")?;
    let mut img = lzx::decompress(compressed, h.kernel_size as usize)?;

    let data_size = u32_at(&img, 0x2c) as usize;
    let data_va = u32_at(&img, 0x34);
    let dst = data_va.wrapping_add(0x7fff_0000) as usize;
    let src = off + h.compressed_size as usize;
    let data = rom
        .get(src..src + data_size)
        .ok_or(".data lies outside the ROM")?;
    img.get_mut(dst..dst + data_size)
        .ok_or(".data lies outside the kernel image")?
        .copy_from_slice(data);
    Ok(img)
}

/// Builds a ROM from `base` with `kernel` packed in, the way EVtool's Pack does.
pub fn pack(kernel: &[u8], base: &[u8]) -> Result<Vec<u8>, String> {
    let mut rom = base.to_vec();
    let mut img = kernel.to_vec();
    let mut h = Header::find(&rom)?;
    let lowest = free_space_start(&rom, h.kernel_offset(rom.len()));

    let mut pos = rom
        .len()
        .checked_sub(h.loader_size as usize + FIXED_BLOCK_SIZE)
        .ok_or("bad loader size")?;

    let data = data_section(&img)?;
    // EVtool stores .data up to and including its last non-zero dword.
    let raw = &img[data.raw_ptr..data.raw_ptr + data.virtual_size];
    let used = (0..raw.len() / 4)
        .rev()
        .find(|&i| u32_at(raw, i * 4) != 0)
        .map_or(0, |i| i * 4 + 4);

    let data_pos = pos.checked_sub(used).ok_or("kernel does not fit")?;
    put_u32(&mut img, 0x28, (data.virtual_size - used) as u32);
    put_u32(&mut img, 0x2c, used as u32);
    put_u32(
        &mut img,
        0x30,
        (data_pos as u32).wrapping_sub(rom.len() as u32),
    );
    put_u32(
        &mut img,
        0x34,
        data.virtual_address.wrapping_sub(0x7fff_0000),
    );
    put_u32(&mut img, 0x38, FIXED_BLOCK_SIZE as u32);
    pos = data_pos;
    rom[pos..pos + used].copy_from_slice(&img[data.raw_ptr..data.raw_ptr + used]);
    img[data.raw_ptr..data.raw_ptr + used].fill(0);

    let compressed = lzx::compress(&img);
    let room = pos - lowest;
    if compressed.len() > room {
        return Err(format!(
            "compressed kernel is {} bytes; only {} fit in this ROM",
            compressed.len(),
            room
        ));
    }
    pos -= compressed.len();
    rom[pos..pos + compressed.len()].copy_from_slice(&compressed);

    h.kernel_addr = 0xff80_0000u32.wrapping_sub((rom.len() - pos) as u32);
    h.compressed_size = compressed.len() as u32;
    h.kernel_size = img.len() as u32;
    h.write(&mut rom);
    Ok(rom)
}

/// The kernel may grow down over the zero padding below it, but not into the
/// init table at the start of the ROM. Keeps 16 bytes of zeros as a margin.
fn free_space_start(rom: &[u8], kernel_offset: usize) -> usize {
    let used = rom[..kernel_offset]
        .iter()
        .rposition(|&b| b != 0)
        .map_or(0, |i| i + 1);
    (used + 16 + 15) & !15
}

struct Section {
    virtual_size: usize,
    virtual_address: u32,
    raw_ptr: usize,
}

fn data_section(img: &[u8]) -> Result<Section, String> {
    let nt = u32_at(img, 0x3c) as usize;
    let count = u16_at(img, nt + 6) as usize;
    let first = nt + 0x18 + u16_at(img, nt + 0x14) as usize;
    (0..count)
        .map(|i| first + i * 0x28)
        .find(|&s| img.get(s..s + 8) == Some(b".data\0\0\0"))
        .map(|s| Section {
            virtual_size: u32_at(img, s + 8) as usize,
            virtual_address: u32_at(img, s + 0xc),
            raw_ptr: u32_at(img, s + 0x14) as usize,
        })
        .ok_or_else(|| "kernel has no .data section".into())
}

pub fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}

fn put_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
