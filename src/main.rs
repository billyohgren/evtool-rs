mod ips;
mod lzx;
mod rom;

use std::fs;
use std::process::ExitCode;

const USAGE: &str = "\
usage:
  evtool-rs unpack <rom.bin> <xboxkrnl.img>
  evtool-rs pack   <xboxkrnl.img> <base-rom.bin> <out.bin>
  evtool-rs ips    <in> <patch.ips> <out>";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["unpack", rom, out] => read(rom)
            .and_then(|r| rom::unpack(&r))
            .and_then(|k| write(out, &k)),
        ["pack", kernel, base, out] => read(kernel)
            .and_then(|k| read(base).and_then(|b| rom::pack(&k, &b)))
            .and_then(|r| write(out, &r)),
        ["ips", input, patch, out] => read(input)
            .and_then(|i| read(patch).and_then(|p| ips::apply(&i, &p)))
            .and_then(|o| write(out, &o)),
        _ => Err(USAGE.into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn read(path: &str) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("{path}: {e}"))
}

fn write(path: &str, data: &[u8]) -> Result<(), String> {
    fs::write(path, data).map_err(|e| format!("{path}: {e}"))
}
