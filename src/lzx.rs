//! The kernel's compressed stream: Microsoft LZX (the LDI/LCI flavour the Xbox
//! uses) with a 128 KB window, cut into 32 KB frames. Each frame is prefixed by
//! `u16 compressed_size, u16 uncompressed_size` and starts on a fresh 16-bit word.
//!
//! The encoder writes one verbatim block per frame and picks its tokens with a
//! cost-based (near-optimal) parse, refined over a few passes. It needs to be about
//! as good as Microsoft's compressor, because the ROM has little room to spare.

const FRAME: usize = 0x8000;
const WINDOW: usize = 0x2_0000;
const MAX_OFFSET: usize = WINDOW - 3;
const POSITION_SLOTS: usize = 34;
const MAIN_SYMBOLS: usize = 256 + 8 * POSITION_SLOTS;
const LENGTH_SYMBOLS: usize = 249;
const MIN_MATCH: usize = 2;
const MAX_MATCH: usize = 257;
const BLOCK_VERBATIM: u32 = 1;
const BLOCK_ALIGNED: u32 = 2;
const BLOCK_UNCOMPRESSED: u32 = 3;

/// Writes E8 translation into the stream header, with this "file size".
/// Microsoft's tools use 12000000; the M8 kernels were compressed with it too.
const E8_SIZE: i32 = 12_000_000;

const FOOTER_BITS: [u32; POSITION_SLOTS] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13, 14, 14, 15, 15,
];

fn base_position(slot: usize) -> u32 {
    (0..slot).map(|s| 1u32 << FOOTER_BITS[s]).sum()
}

fn position_slot(formatted: u32) -> usize {
    (0..POSITION_SLOTS)
        .rev()
        .find(|&s| base_position(s) <= formatted)
        .unwrap()
}

// ---------------------------------------------------------------- bit I/O

struct BitReader<'a> {
    src: &'a [u8],
    pos: usize,
    buf: u32,
    bits: u32,
}

impl<'a> BitReader<'a> {
    fn new(src: &'a [u8]) -> Self {
        BitReader {
            src,
            pos: 0,
            buf: 0,
            bits: 0,
        }
    }

    fn fill(&mut self) {
        while self.bits <= 16 {
            let w = match self.src.get(self.pos..self.pos + 2) {
                Some(b) => u16::from_le_bytes([b[0], b[1]]) as u32,
                None => 0,
            };
            self.pos += 2;
            self.buf |= w << (16 - self.bits);
            self.bits += 16;
        }
    }

    fn peek(&mut self, n: u32) -> u32 {
        self.fill();
        self.buf >> (32 - n)
    }

    fn skip(&mut self, n: u32) {
        self.fill();
        self.buf <<= n;
        self.bits -= n;
    }

    fn read(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        if n > 16 {
            let hi = self.read(16);
            return hi << (n - 16) | self.read(n - 16);
        }
        let v = self.peek(n);
        self.skip(n);
        v
    }

    fn overrun(&self) -> bool {
        self.pos > self.src.len() + 2
    }
}

#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    bits: u32,
}

impl BitWriter {
    fn put(&mut self, v: u32, n: u32) {
        if n > 16 {
            self.put(v >> 16, n - 16);
            self.put(v & 0xffff, 16);
            return;
        }
        self.acc = self.acc << n | (v & ((1 << n) - 1));
        self.bits += n;
        while self.bits >= 16 {
            let w = (self.acc >> (self.bits - 16)) as u16;
            self.out.extend_from_slice(&w.to_le_bytes());
            self.bits -= 16;
            self.acc &= (1 << self.bits) - 1;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.put(0, 16 - self.bits);
        }
        self.out
    }
}

// ---------------------------------------------------------------- Huffman

struct Decoder {
    lens: Vec<u8>,
    max: u32,
    table: Vec<u16>,
}

impl Decoder {
    fn new(lens: &[u8]) -> Result<Option<Decoder>, String> {
        let max = *lens.iter().max().unwrap() as u32;
        if max == 0 {
            return Ok(None);
        }
        let mut table = vec![0u16; 1 << max];
        let mut pos = 0;
        for len in 1..=max {
            for (sym, _) in lens.iter().enumerate().filter(|&(_, &l)| l as u32 == len) {
                let n = 1 << (max - len);
                table
                    .get_mut(pos..pos + n)
                    .ok_or("bad Huffman lengths")?
                    .fill(sym as u16);
                pos += n;
            }
        }
        if pos != table.len() {
            return Err("incomplete Huffman tree".into());
        }
        Ok(Some(Decoder {
            lens: lens.to_vec(),
            max,
            table,
        }))
    }

    fn decode(&self, r: &mut BitReader) -> usize {
        let sym = self.table[r.peek(self.max) as usize] as usize;
        r.skip(self.lens[sym] as u32);
        sym
    }
}

/// Code lengths for `freq`, no longer than `limit`. At least two symbols get a
/// code so the tree is always complete.
fn huffman_lengths(freq: &[u32], limit: u8) -> Vec<u8> {
    let mut f: Vec<u32> = freq.to_vec();
    for i in 0..f.len() {
        if f.iter().filter(|&&x| x > 0).count() >= 2 {
            break;
        }
        if f[i] == 0 {
            f[i] = 1;
        }
    }
    loop {
        let lens = huffman_unlimited(&f);
        if lens.iter().all(|&l| l <= limit) {
            return lens;
        }
        for x in f.iter_mut().filter(|x| **x > 0) {
            *x = (*x).div_ceil(2);
        }
    }
}

fn huffman_unlimited(freq: &[u32]) -> Vec<u8> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    // Nodes: leaves 0..n, internal nodes after. parent[] gives depth.
    let n = freq.len();
    let mut parent = vec![usize::MAX; 2 * n];
    let mut heap: BinaryHeap<Reverse<(u64, usize)>> = freq
        .iter()
        .enumerate()
        .filter(|&(_, &f)| f > 0)
        .map(|(i, &f)| Reverse((f as u64, i)))
        .collect();
    let mut next = n;
    while heap.len() > 1 {
        let Reverse((fa, a)) = heap.pop().unwrap();
        let Reverse((fb, b)) = heap.pop().unwrap();
        parent[a] = next;
        parent[b] = next;
        heap.push(Reverse((fa + fb, next)));
        next += 1;
    }
    (0..n)
        .map(|i| {
            if freq[i] == 0 {
                return 0;
            }
            let (mut d, mut p) = (0u8, i);
            while parent[p] != usize::MAX {
                p = parent[p];
                d += 1;
            }
            d
        })
        .collect()
}

fn canonical_codes(lens: &[u8]) -> Vec<u32> {
    let mut codes = vec![0u32; lens.len()];
    let mut code = 0u32;
    for len in 1..=16u8 {
        for (sym, _) in lens.iter().enumerate().filter(|&(_, &l)| l == len) {
            codes[sym] = code;
            code += 1;
        }
        code <<= 1;
    }
    codes
}

// ---------------------------------------------------------------- decoder

struct Trees {
    main: Vec<u8>,
    length: Vec<u8>,
}

fn read_lengths(r: &mut BitReader, lens: &mut [u8]) -> Result<(), String> {
    let pre: Vec<u8> = (0..20).map(|_| r.read(4) as u8).collect();
    let pre = Decoder::new(&pre)?.ok_or("empty pretree")?;
    let mut i = 0;
    while i < lens.len() {
        let delta = |old: u8, code: usize| ((17 + old as usize - code) % 17) as u8;
        match pre.decode(r) {
            code @ 0..=16 => {
                lens[i] = delta(lens[i], code);
                i += 1;
            }
            17 => {
                let n = 4 + r.read(4) as usize;
                lens.get_mut(i..i + n)
                    .ok_or("pretree run too long")?
                    .fill(0);
                i += n;
            }
            18 => {
                let n = 20 + r.read(5) as usize;
                lens.get_mut(i..i + n)
                    .ok_or("pretree run too long")?
                    .fill(0);
                i += n;
            }
            _ => {
                let n = 4 + r.read(1) as usize;
                let code = pre.decode(r);
                if code > 16 {
                    return Err("bad pretree code".into());
                }
                let v = delta(lens[i], code);
                lens.get_mut(i..i + n)
                    .ok_or("pretree run too long")?
                    .fill(v);
                i += n;
            }
        }
    }
    Ok(())
}

pub fn decompress(src: &[u8], size: usize) -> Result<Vec<u8>, String> {
    let mut out: Vec<u8> = Vec::with_capacity(size);
    let mut plain: Vec<u8> = Vec::with_capacity(size);
    let mut trees = Trees {
        main: vec![0; MAIN_SYMBOLS],
        length: vec![0; LENGTH_SYMBOLS],
    };
    let mut rep = [1u32; 3];
    let mut e8: Option<i32> = None;
    let mut block_left = 0usize;
    let mut aligned: Option<Decoder> = None;
    let (mut main, mut length): (Option<Decoder>, Option<Decoder>) = (None, None);

    let mut p = 0;
    while out.len() < size {
        let head = src.get(p..p + 4).ok_or("compressed kernel is truncated")?;
        let csize = u16::from_le_bytes([head[0], head[1]]) as usize;
        let usize_ = u16::from_le_bytes([head[2], head[3]]) as usize;
        let chunk = src
            .get(p + 4..p + 4 + csize)
            .ok_or("compressed frame is truncated")?;
        let mut r = BitReader::new(chunk);
        let frame_start = out.len();

        if frame_start == 0 && r.read(1) == 1 {
            e8 = Some(r.read(32) as i32);
        }
        while out.len() - frame_start < usize_ {
            if block_left == 0 {
                let block_type = r.read(3);
                block_left = r.read(24) as usize;
                if block_left == 0 {
                    return Err("zero-sized LZX block".into());
                }
                match block_type {
                    BLOCK_VERBATIM | BLOCK_ALIGNED => {
                        aligned = None;
                        if block_type == BLOCK_ALIGNED {
                            let l: Vec<u8> = (0..8).map(|_| r.read(3) as u8).collect();
                            aligned = Decoder::new(&l)?;
                        }
                        read_lengths(&mut r, &mut trees.main[..256])?;
                        read_lengths(&mut r, &mut trees.main[256..])?;
                        read_lengths(&mut r, &mut trees.length)?;
                        main = Decoder::new(&trees.main)?;
                        length = Decoder::new(&trees.length)?;
                    }
                    BLOCK_UNCOMPRESSED => {
                        return Err("uncompressed LZX blocks are not supported".into());
                    }
                    t => return Err(format!("bad LZX block type {t}")),
                }
            }
            let main = main.as_ref().ok_or("empty main tree")?;
            let sym = main.decode(&mut r);
            let n = if sym < 256 {
                out.push(sym as u8);
                1
            } else {
                let mut len = (sym - 256) & 7;
                if len == 7 {
                    len += length.as_ref().ok_or("empty length tree")?.decode(&mut r);
                }
                let len = len + MIN_MATCH;
                let slot = (sym - 256) >> 3;
                let offset = match slot {
                    0 => rep[0],
                    1 => {
                        rep.swap(0, 1);
                        rep[0]
                    }
                    2 => {
                        rep.swap(0, 2);
                        rep[0]
                    }
                    _ => {
                        let bits = FOOTER_BITS[slot];
                        let extra = match &aligned {
                            Some(a) if bits >= 3 => {
                                (r.read(bits - 3) << 3) + a.decode(&mut r) as u32
                            }
                            _ => r.read(bits),
                        };
                        let o = base_position(slot) + extra - 2;
                        rep = [o, rep[0], rep[1]];
                        o
                    }
                } as usize;
                if offset == 0 || offset > out.len() {
                    return Err(format!(
                        "LZX match reaches before the start (offset {offset})"
                    ));
                }
                for _ in 0..len {
                    out.push(out[out.len() - offset]);
                }
                len
            };
            block_left = block_left
                .checked_sub(n)
                .ok_or("LZX token overruns its block")?;
            if r.overrun() {
                return Err("LZX frame overruns its compressed size".into());
            }
        }
        if out.len() - frame_start != usize_ {
            return Err("LZX match crosses a frame boundary".into());
        }
        // Matches copy from the translated stream, so untranslate a copy.
        let mut frame = out[frame_start..].to_vec();
        if let Some(e8) = e8 {
            e8_decode(&mut frame, frame_start, e8);
        }
        plain.extend_from_slice(&frame);
        p += 4 + csize;
    }
    plain.truncate(size);
    Ok(plain)
}

/// Undoes E8 translation on one frame, as Microsoft's decoders do.
fn e8_decode(frame: &mut [u8], frame_start: usize, size: i32) {
    if frame.len() <= 10 || frame_start >= 0x4000_0000 {
        return;
    }
    let mut i = 0;
    while i < frame.len() - 10 {
        if frame[i] != 0xe8 {
            i += 1;
            continue;
        }
        let cur = (frame_start + i) as i32;
        let v = i32::from_le_bytes(frame[i + 1..i + 5].try_into().unwrap());
        if v >= -cur && v < size {
            let rel = if v >= 0 { v - cur } else { v + size };
            frame[i + 1..i + 5].copy_from_slice(&rel.to_le_bytes());
        }
        i += 5;
    }
}

fn e8_encode(frame: &mut [u8], frame_start: usize, size: i32) {
    if frame.len() <= 10 || frame_start >= 0x4000_0000 {
        return;
    }
    let mut i = 0;
    while i < frame.len() - 10 {
        if frame[i] != 0xe8 {
            i += 1;
            continue;
        }
        let cur = (frame_start + i) as i32;
        let rel = i32::from_le_bytes(frame[i + 1..i + 5].try_into().unwrap());
        if rel >= -cur && rel < size {
            let v = if rel < size - cur {
                rel + cur
            } else {
                rel - size
            };
            frame[i + 1..i + 5].copy_from_slice(&v.to_le_bytes());
        }
        i += 5;
    }
}

// ---------------------------------------------------------------- encoder

#[derive(Clone, Copy)]
enum Token {
    Literal(u8),
    /// `slot` 0..2 are repeat offsets; otherwise `offset` is a new one.
    Match {
        len: u16,
        slot: u8,
        offset: u32,
    },
}

impl Token {
    fn len(&self) -> usize {
        match *self {
            Token::Literal(_) => 1,
            Token::Match { len, .. } => len as usize,
        }
    }
}

/// Every match worth considering at a position: strictly increasing lengths,
/// each with the nearest offset that reaches it.
struct MatchFinder {
    head: Vec<u32>,
    prev: Vec<u32>,
    depth: usize,
}

const HASH_BITS: u32 = 16;
const NIL: u32 = u32::MAX;

impl MatchFinder {
    fn new(len: usize, depth: usize) -> Self {
        MatchFinder {
            head: vec![NIL; 1 << HASH_BITS],
            prev: vec![NIL; len],
            depth,
        }
    }

    fn hash(d: &[u8], i: usize) -> usize {
        let v = (d[i] as u32) << 16 | (d[i + 1] as u32) << 8 | d[i + 2] as u32;
        (v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
    }

    /// Inserts position `i` and returns its matches, each capped at `limit` bytes.
    fn find(&mut self, d: &[u8], i: usize, limit: usize, out: &mut Vec<(u16, u32)>) {
        out.clear();
        if i + 3 > d.len() {
            return;
        }
        let h = Self::hash(d, i);
        let mut cand = self.head[h];
        self.head[h] = i as u32;
        self.prev[i] = cand;
        let mut best = 2;
        let mut steps = 0;
        while cand != NIL && steps < self.depth {
            let c = cand as usize;
            let off = i - c;
            if off > MAX_OFFSET {
                break;
            }
            if d[c + best.min(limit - 1)] == d[i + best.min(limit - 1)] {
                let l = d[c..]
                    .iter()
                    .zip(&d[i..i + limit])
                    .take_while(|(a, b)| a == b)
                    .count();
                if l > best {
                    best = l;
                    out.push((l as u16, off as u32));
                    if l >= limit {
                        break;
                    }
                }
            }
            cand = self.prev[c];
            steps += 1;
        }
    }
}

struct Costs {
    main: Vec<u32>,
    length: Vec<u32>,
    aligned: Option<Vec<u32>>,
}

impl Costs {
    fn initial() -> Self {
        Costs {
            main: (0..MAIN_SYMBOLS)
                .map(|s| if s < 256 { 8 } else { 9 })
                .collect(),
            length: vec![6; LENGTH_SYMBOLS],
            aligned: None,
        }
    }

    fn from_code(code: &Code) -> Self {
        let c = |l: &u8| if *l == 0 { 18 } else { *l as u32 };
        Costs {
            main: code.main.iter().map(c).collect(),
            length: code.length.iter().map(c).collect(),
            aligned: code.aligned.as_ref().map(|a| a.iter().map(c).collect()),
        }
    }

    fn match_cost(&self, len: usize, slot: usize, offset: u32) -> u32 {
        let lh = (len - MIN_MATCH).min(7);
        let mut c = self.main[256 + slot * 8 + lh];
        if lh == 7 {
            c += self.length[len - MIN_MATCH - 7];
        }
        c + self.footer_cost(slot, offset)
    }

    fn footer_cost(&self, slot: usize, offset: u32) -> u32 {
        if slot < 3 {
            return 0;
        }
        let bits = FOOTER_BITS[slot];
        match &self.aligned {
            Some(a) if bits >= 3 => bits - 3 + a[(offset as usize + 2) & 7],
            _ => bits,
        }
    }
}

#[derive(Clone, Copy)]
struct Node {
    cost: u32,
    token: Option<Token>,
    rep: [u32; 3],
}

/// Cheapest tokenisation of `d[start..end]`, costed per segment.
fn parse(
    d: &[u8],
    start: usize,
    end: usize,
    matches: &[Vec<(u16, u32)>],
    seg_costs: &[Costs],
    rep0: [u32; 3],
) -> Vec<Token> {
    let n = end - start;
    let mut nodes = vec![
        Node {
            cost: u32::MAX,
            token: None,
            rep: [1; 3]
        };
        n + 1
    ];
    nodes[0] = Node {
        cost: 0,
        token: None,
        rep: rep0,
    };
    for i in 0..n {
        let node = nodes[i];
        if node.cost == u32::MAX {
            continue;
        }
        let pos = start + i;
        let costs = &seg_costs[pos / SEGMENT];
        let limit = (n - i).min(MAX_MATCH);
        let mut relax = |j: usize, cost: u32, token: Token, rep: [u32; 3]| {
            if cost < nodes[j].cost {
                nodes[j] = Node {
                    cost,
                    token: Some(token),
                    rep,
                };
            }
        };

        let lit = d[pos];
        relax(
            i + 1,
            node.cost + costs.main[lit as usize],
            Token::Literal(lit),
            node.rep,
        );

        let mut rep_best = 0;
        for k in 0..3 {
            let off = node.rep[k] as usize;
            if off > pos {
                continue;
            }
            let l = d[pos - off..]
                .iter()
                .zip(&d[pos..pos + limit])
                .take_while(|(a, b)| a == b)
                .count();
            let mut rep = node.rep;
            rep.swap(0, k);
            for len in MIN_MATCH..=l {
                let token = Token::Match {
                    len: len as u16,
                    slot: k as u8,
                    offset: off as u32,
                };
                relax(i + len, node.cost + costs.match_cost(len, k, 0), token, rep);
            }
            rep_best = rep_best.max(l);
        }

        let mut len = 3;
        for &(ml, off) in &matches[i] {
            let ml = ml as usize;
            let slot = position_slot(off + 2);
            let rep = [off, node.rep[0], node.rep[1]];
            while len <= ml {
                // A repeat offset match of this length is never worse.
                if len > rep_best {
                    let token = Token::Match {
                        len: len as u16,
                        slot: slot as u8,
                        offset: off,
                    };
                    relax(
                        i + len,
                        node.cost + costs.match_cost(len, slot, off),
                        token,
                        rep,
                    );
                }
                len += 1;
            }
        }
    }

    let mut tokens = Vec::new();
    let mut j = n;
    while j > 0 {
        let t = nodes[j].token.unwrap();
        j -= t.len();
        tokens.push(t);
    }
    tokens.reverse();
    tokens
}

fn advance_rep(rep: &mut [u32; 3], tokens: &[Token]) {
    for t in tokens {
        if let Token::Match { slot, offset, .. } = *t {
            match slot {
                0 => {}
                1 => rep.swap(0, 1),
                2 => rep.swap(0, 2),
                _ => *rep = [offset, rep[0], rep[1]],
            }
        }
    }
}

/// Symbol statistics of a run of tokens.
#[derive(Clone)]
struct Hist {
    main: Vec<u32>,
    length: Vec<u32>,
    aligned: [u32; 8],
    /// Footer bits if sent verbatim.
    footer_bits: u64,
    /// Offsets whose low 3 bits an aligned block would Huffman-code.
    aligned_count: u64,
    bytes: usize,
}

impl Hist {
    fn new() -> Hist {
        Hist {
            main: vec![0; MAIN_SYMBOLS],
            length: vec![0; LENGTH_SYMBOLS],
            aligned: [0; 8],
            footer_bits: 0,
            aligned_count: 0,
            bytes: 0,
        }
    }

    fn add(&mut self, t: &Token) {
        self.bytes += t.len();
        match *t {
            Token::Literal(b) => self.main[b as usize] += 1,
            Token::Match { len, slot, offset } => {
                let lh = (len as usize - MIN_MATCH).min(7);
                self.main[256 + slot as usize * 8 + lh] += 1;
                if lh == 7 {
                    self.length[len as usize - MIN_MATCH - 7] += 1;
                }
                if slot >= 3 {
                    let bits = FOOTER_BITS[slot as usize];
                    self.footer_bits += bits as u64;
                    if bits >= 3 {
                        self.aligned[(offset as usize + 2) & 7] += 1;
                        self.aligned_count += 1;
                    }
                }
            }
        }
    }

    fn merged(&self, other: &Hist) -> Hist {
        let mut h = self.clone();
        h.main
            .iter_mut()
            .zip(&other.main)
            .for_each(|(a, b)| *a += b);
        h.length
            .iter_mut()
            .zip(&other.length)
            .for_each(|(a, b)| *a += b);
        h.aligned
            .iter_mut()
            .zip(&other.aligned)
            .for_each(|(a, b)| *a += b);
        h.footer_bits += other.footer_bits;
        h.aligned_count += other.aligned_count;
        h.bytes += other.bytes;
        h
    }
}

/// The Huffman code of one block.
#[derive(Clone)]
struct Code {
    main: Vec<u8>,
    length: Vec<u8>,
    /// Present for an aligned offset block.
    aligned: Option<Vec<u8>>,
}

impl Code {
    fn build(h: &Hist) -> Code {
        let alen = huffman_lengths(&h.aligned, 7);
        // An aligned block costs 24 header bits and saves (3 - length) per offset.
        let saved: i64 = h
            .aligned
            .iter()
            .zip(&alen)
            .map(|(&f, &l)| f as i64 * (3 - l as i64))
            .sum();
        Code {
            main: huffman_lengths(&h.main, 16),
            length: huffman_lengths(&h.length, 16),
            aligned: (saved > 24).then_some(alen),
        }
    }

    /// Bits for a block with this code: header, trees and tokens.
    fn bits(&self, h: &Hist, prev: &Trees) -> u64 {
        let mut w = BitWriter::default();
        let mut p = Trees {
            main: prev.main.clone(),
            length: prev.length.clone(),
        };
        write_block_header(&mut w, self, 1, &mut p);
        let dot = |f: &[u32], l: &[u8]| {
            f.iter()
                .zip(l)
                .map(|(&f, &l)| f as u64 * l as u64)
                .sum::<u64>()
        };
        let footer = match &self.aligned {
            Some(a) => h.footer_bits - 3 * h.aligned_count + dot(&h.aligned, a),
            None => h.footer_bits,
        };
        (w.out.len() * 8) as u64
            + w.bits as u64
            + dot(&h.main, &self.main)
            + dot(&h.length, &self.length)
            + footer
    }
}

/// Writes code lengths as pretree-coded deltas from the previous block's.
fn write_lengths(w: &mut BitWriter, prev: &[u8], lens: &[u8]) {
    // Pretree symbols with their extra bits.
    let mut syms: Vec<(u8, u32, u32)> = Vec::new();
    let mut i = 0;
    while i < lens.len() {
        if lens[i] == 0 {
            let run = lens[i..].iter().take_while(|&&l| l == 0).count();
            if run >= 20 {
                let n = run.min(51);
                syms.push((18, (n - 20) as u32, 5));
                i += n;
                continue;
            }
            if run >= 4 {
                syms.push((17, (run - 4) as u32, 4));
                i += run;
                continue;
            }
        }
        syms.push((
            ((17 + prev[i] as usize - lens[i] as usize) % 17) as u8,
            0,
            0,
        ));
        i += 1;
    }
    let mut freq = [0u32; 20];
    for s in &syms {
        freq[s.0 as usize] += 1;
    }
    let pre = huffman_lengths(&freq, 15);
    let codes = canonical_codes(&pre);
    for &l in &pre {
        w.put(l as u32, 4);
    }
    for (sym, extra, bits) in syms {
        w.put(codes[sym as usize], pre[sym as usize] as u32);
        w.put(extra, bits);
    }
}

/// Writes a block's header and trees; `prev` holds the previous block's lengths.
fn write_block_header(w: &mut BitWriter, code: &Code, size: usize, prev: &mut Trees) {
    match &code.aligned {
        Some(a) => {
            w.put(BLOCK_ALIGNED, 3);
            w.put(size as u32, 24);
            for &l in a {
                w.put(l as u32, 3);
            }
        }
        None => {
            w.put(BLOCK_VERBATIM, 3);
            w.put(size as u32, 24);
        }
    }
    write_lengths(w, &prev.main[..256], &code.main[..256]);
    write_lengths(w, &prev.main[256..], &code.main[256..]);
    write_lengths(w, &prev.length, &code.length);
    prev.main.copy_from_slice(&code.main);
    prev.length.copy_from_slice(&code.length);
}

fn write_tokens(w: &mut BitWriter, code: &Code, tokens: &[Token]) {
    let mc = canonical_codes(&code.main);
    let lc = canonical_codes(&code.length);
    let ac = code.aligned.as_ref().map(|a| canonical_codes(a));
    for t in tokens {
        match *t {
            Token::Literal(b) => w.put(mc[b as usize], code.main[b as usize] as u32),
            Token::Match { len, slot, offset } => {
                let lh = (len as usize - MIN_MATCH).min(7);
                let sym = 256 + slot as usize * 8 + lh;
                w.put(mc[sym], code.main[sym] as u32);
                if lh == 7 {
                    let s = len as usize - MIN_MATCH - 7;
                    w.put(lc[s], code.length[s] as u32);
                }
                if slot >= 3 {
                    let slot = slot as usize;
                    let bits = FOOTER_BITS[slot];
                    let extra = offset + 2 - base_position(slot);
                    match (&code.aligned, &ac) {
                        (Some(a), Some(ac)) if bits >= 3 => {
                            w.put(extra >> 3, bits - 3);
                            let s = (extra & 7) as usize;
                            w.put(ac[s], a[s] as u32);
                        }
                        _ => w.put(extra, bits),
                    }
                }
            }
        }
    }
}

/// Bytes per segment, the unit blocks are built from.
const SEGMENT: usize = 512;

/// Groups segments into blocks: start with one block per segment, then keep
/// merging the neighbouring pair that saves the most bits.
fn partition(segs: &[Hist]) -> Vec<(usize, usize, Code)> {
    let zero = Trees {
        main: vec![0; MAIN_SYMBOLS],
        length: vec![0; LENGTH_SYMBOLS],
    };
    let cost = |h: &Hist| Code::build(h).bits(h, &zero);
    // (first segment, end segment, histogram, cost)
    let mut blocks: Vec<(usize, usize, Hist, u64)> = segs
        .iter()
        .enumerate()
        .map(|(i, h)| (i, i + 1, h.clone(), cost(h)))
        .collect();
    let mut gains: Vec<(i64, Hist, u64)> = Vec::new();
    let pair = |a: &(usize, usize, Hist, u64), b: &(usize, usize, Hist, u64)| {
        let m = a.2.merged(&b.2);
        let c = cost(&m);
        ((a.3 + b.3) as i64 - c as i64, m, c)
    };
    for w in blocks.windows(2) {
        gains.push(pair(&w[0], &w[1]));
    }
    while let Some((i, _)) = gains
        .iter()
        .enumerate()
        .filter(|(_, g)| g.0 > 0)
        .max_by_key(|(_, g)| g.0)
    {
        let (_, m, c) = gains.remove(i);
        let end = blocks[i + 1].1;
        blocks.remove(i + 1);
        blocks[i] = (blocks[i].0, end, m, c);
        if i + 1 < blocks.len() {
            gains[i] = pair(&blocks[i], &blocks[i + 1]);
        }
        if i > 0 {
            gains[i - 1] = pair(&blocks[i - 1], &blocks[i]);
        }
    }
    blocks
        .into_iter()
        .map(|(a, b, h, _)| (a, b, Code::build(&h)))
        .collect()
}

pub fn compress(src: &[u8]) -> Vec<u8> {
    let mut d = src.to_vec();
    for (f, chunk) in d.chunks_mut(FRAME).enumerate() {
        e8_encode(chunk, f * FRAME, E8_SIZE);
    }

    let mut finder = MatchFinder::new(d.len(), 1024);
    let mut found = Vec::new();
    let ranges: Vec<(usize, usize)> = (0..d.len())
        .step_by(FRAME)
        .map(|s| (s, (s + FRAME).min(d.len())))
        .collect();
    let matches: Vec<Vec<Vec<(u16, u32)>>> = ranges
        .iter()
        .map(|&(s, e)| {
            (s..e)
                .map(|i| {
                    finder.find(&d, i, (e - i).min(MAX_MATCH), &mut found);
                    found.clone()
                })
                .collect()
        })
        .collect();

    let nsegs = d.len().div_ceil(SEGMENT);
    let mut costs: Vec<Costs> = (0..nsegs).map(|_| Costs::initial()).collect();
    let mut best: Option<Vec<u8>> = None;
    for _ in 0..6 {
        let mut rep = [1u32; 3];
        let frames: Vec<Vec<Token>> = ranges
            .iter()
            .enumerate()
            .map(|(f, &(s, e))| {
                let t = parse(&d, s, e, &matches[f], &costs, rep);
                advance_rep(&mut rep, &t);
                t
            })
            .collect();
        let mut segs = vec![Hist::new(); nsegs];
        for (&(s, _), tokens) in ranges.iter().zip(&frames) {
            let mut pos = s;
            for t in tokens {
                segs[pos / SEGMENT].add(t);
                pos += t.len();
            }
        }
        let blocks = partition(&segs);
        let out = write_stream(&ranges, &frames, &blocks);
        for (a, b, code) in &blocks {
            for c in &mut costs[*a..*b] {
                *c = Costs::from_code(code);
            }
        }
        if best.as_ref().is_none_or(|b| out.len() < b.len()) {
            best = Some(out);
        }
    }
    best.unwrap()
}

/// Writes the frames; a block starts at the first token of its first segment.
fn write_stream(
    ranges: &[(usize, usize)],
    frames: &[Vec<Token>],
    blocks: &[(usize, usize, Code)],
) -> Vec<u8> {
    // Block sizes in bytes, from where its tokens start.
    let mut sizes = vec![0usize; blocks.len()];
    let block_of = |seg: usize| blocks.partition_point(|b| b.1 <= seg);
    for (&(s, _), tokens) in ranges.iter().zip(frames) {
        let mut pos = s;
        for t in tokens {
            sizes[block_of(pos / SEGMENT)] += t.len();
            pos += t.len();
        }
    }

    let mut prev = Trees {
        main: vec![0; MAIN_SYMBOLS],
        length: vec![0; LENGTH_SYMBOLS],
    };
    let mut out = Vec::new();
    let mut current = usize::MAX;
    for (f, (&(s, e), tokens)) in ranges.iter().zip(frames).enumerate() {
        let mut w = BitWriter::default();
        if f == 0 {
            w.put(1, 1);
            w.put(E8_SIZE as u32, 32);
        }
        let mut pos = s;
        let mut run_start = 0;
        for (i, t) in tokens.iter().enumerate() {
            let b = block_of(pos / SEGMENT);
            if b != current {
                if current != usize::MAX {
                    write_tokens(&mut w, &blocks[current].2, &tokens[run_start..i]);
                }
                run_start = i;
                current = b;
                write_block_header(&mut w, &blocks[b].2, sizes[b], &mut prev);
            }
            pos += t.len();
        }
        write_tokens(&mut w, &blocks[current].2, &tokens[run_start..]);
        let bytes = w.finish();
        out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&((e - s) as u16).to_le_bytes());
        out.extend_from_slice(&bytes);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        let mut v = Vec::new();
        let mut x = 1u32;
        for i in 0..200_000u32 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            match (x >> 16) % 7 {
                0 => v.extend_from_slice(&[0xe8, (i & 0xff) as u8, (i >> 8) as u8, 0, 0]),
                1 => v.extend_from_slice(b"KeBugCheck"),
                2 => v.push(0),
                _ => v.push((x >> 24) as u8),
            }
        }
        v
    }

    #[test]
    fn round_trip() {
        let d = sample();
        let c = compress(&d);
        assert_eq!(decompress(&c, d.len()).unwrap(), d);
    }

    #[test]
    fn lzxd_agrees() {
        // lzxd treats an E8 value of exactly 0 differently from Microsoft, so
        // compare on data that never produces one.
        let d: Vec<u8> = sample()
            .into_iter()
            .map(|b| if b == 0xe8 { 0xe9 } else { b })
            .collect();
        let c = compress(&d);
        let mut lzx = lzxd::Lzxd::new(lzxd::WindowSize::KB128);
        let mut out = Vec::new();
        let mut p = 0;
        while p < c.len() {
            let cs = u16::from_le_bytes([c[p], c[p + 1]]) as usize;
            let us = u16::from_le_bytes([c[p + 2], c[p + 3]]) as usize;
            out.extend_from_slice(lzx.decompress_next(&c[p + 4..p + 4 + cs], us).unwrap());
            p += 4 + cs;
        }
        assert_eq!(out, d);
    }
}
