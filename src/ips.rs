//! IPS patches: "PATCH", then records of `u24 offset, u16 size, data`
//! (size 0 means `u16 count, u8 value` run-length fill), then "EOF".

pub fn apply(input: &[u8], patch: &[u8]) -> Result<Vec<u8>, String> {
    let body = patch.strip_prefix(b"PATCH").ok_or("not an IPS patch")?;
    let mut out = input.to_vec();
    let mut p = 0;
    let take = |p: &mut usize, n: usize| -> Result<&[u8], String> {
        let s = body.get(*p..*p + n).ok_or("IPS patch is truncated")?;
        *p += n;
        Ok(s)
    };
    loop {
        let head = take(&mut p, 3)?;
        if head == b"EOF" {
            break;
        }
        let offset = (head[0] as usize) << 16 | (head[1] as usize) << 8 | head[2] as usize;
        let size = take(&mut p, 2)?;
        let size = (size[0] as usize) << 8 | size[1] as usize;
        let data = if size == 0 {
            let rle = take(&mut p, 3)?;
            vec![rle[2]; (rle[0] as usize) << 8 | rle[1] as usize]
        } else {
            take(&mut p, size)?.to_vec()
        };
        if offset + data.len() > input.len() {
            return Err(format!(
                "IPS record at 0x{offset:x} runs past the end of the input"
            ));
        }
        out[offset..offset + data.len()].copy_from_slice(&data);
    }
    // A truncation size may follow "EOF"; these patches don't use it.
    if p != body.len() {
        return Err("unexpected data after IPS EOF".into());
    }
    Ok(out)
}
