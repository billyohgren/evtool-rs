//! Decodes a packed ROM's kernel with the independent `lzxd` crate.
//! Run with: ROM=<rom.bin> IMG=<expected kernel> cargo test --release -- --ignored

#[test]
#[ignore]
fn lzxd_decodes_packed_rom() {
    let rom = std::fs::read(std::env::var("ROM").unwrap()).unwrap();
    let want = std::fs::read(std::env::var("IMG").unwrap()).unwrap();
    let h = rom.windows(10).rposition(|w| w == b"$EvoxRom$\0").unwrap();
    let u = |o: usize| u32::from_le_bytes(rom[o..o + 4].try_into().unwrap()) as usize;
    let off = (u(h + 0xe) + rom.len()).wrapping_sub(0x80_0000) & 0xff_ffff;
    let (csize, ksize) = (u(h + 0x12), u(h + 0x16));
    let src = &rom[off..off + csize];
    let mut lzx = lzxd::Lzxd::new(lzxd::WindowSize::KB128);
    let mut img = Vec::new();
    let mut p = 0;
    while img.len() < ksize {
        let cs = u16::from_le_bytes([src[p], src[p + 1]]) as usize;
        let us = u16::from_le_bytes([src[p + 2], src[p + 3]]) as usize;
        img.extend_from_slice(lzx.decompress_next(&src[p + 4..p + 4 + cs], us).unwrap());
        p += 4 + cs;
    }
    // Merge .data back in, as unpack does.
    let k = |o: usize| u32::from_le_bytes(img[o..o + 4].try_into().unwrap()) as usize;
    let (dsize, dst) = (k(0x2c), k(0x34).wrapping_add(0x7fff_0000) & 0xffff_ffff);
    img[dst..dst + dsize].copy_from_slice(&rom[off + csize..off + csize + dsize]);
    let diffs: Vec<usize> = (0..ksize).filter(|&i| img[i] != want[i]).collect();
    println!("{} differing bytes", diffs.len());
    for &i in diffs.iter().take(20) {
        println!("  0x{i:x}: lzxd {:02x} want {:02x}", img[i], want[i]);
    }
    assert!(diffs.is_empty());
}
