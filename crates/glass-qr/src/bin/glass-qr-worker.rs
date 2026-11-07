//! Decodes frames from stdin and answers on stdout, as `glass_qr::worker` describes. It holds no
//! state but its buffers, logs nothing, and exits on the end of its input or on anything out of
//! protocol. It is meant to be killed: see `glass_qr::worker`.

use std::io::{self, Read, Write};
use std::process::ExitCode;

use glass_qr::scan::{DECODER_MAX_GRIDS, Frame, Scanner, check_size};
use glass_qr::setup::MAX_TEXT_BYTES;
use glass_qr::worker::MAGIC;
use zeroize::Zeroizing;

fn main() -> ExitCode {
    match serve(io::stdin().lock(), io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(2),
    }
}

fn serve(mut input: impl Read, mut output: impl Write) -> io::Result<()> {
    let mut scanner = Scanner::new();
    let mut pixels = Zeroizing::new(Vec::new());
    loop {
        let mut header = [0u8; 12];
        match input.read_exact(&mut header) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            r => r?,
        }
        if &header[..4] != MAGIC {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let width = u32::from_le_bytes(header[4..8].try_into().expect("4 bytes")) as usize;
        let height = u32::from_le_bytes(header[8..].try_into().expect("4 bytes")) as usize;
        check_size(width, height).map_err(|_| io::ErrorKind::InvalidData)?;
        let n = width * height;
        if pixels.len() != n {
            // A new buffer, so that the old one is wiped as it goes.
            pixels = Zeroizing::new(vec![0u8; n]);
        }
        input.read_exact(&mut pixels)?;
        let frame = Frame::new(&pixels, width, height, width).expect("checked above");
        let codes = scanner.read_codes(frame);
        // Sized for the largest answer, so that it never moves and leaves copies behind.
        let mut answer = Zeroizing::new(Vec::with_capacity(
            5 + DECODER_MAX_GRIDS * (2 + MAX_TEXT_BYTES),
        ));
        // quircs keeps within these; the other side refuses more, so clamping cannot hide anything.
        answer.push(codes.grids.min(u8::MAX.into()) as u8);
        answer.push(codes.texts.len() as u8);
        answer.push(codes.capstones.min(u8::MAX.into()) as u8);
        answer.extend_from_slice(&(codes.regions.min(u16::MAX.into()) as u16).to_le_bytes());
        for t in &codes.texts {
            answer.extend_from_slice(&(t.len() as u16).to_le_bytes());
            answer.extend_from_slice(t);
        }
        output.write_all(&answer)?;
        output.flush()?;
    }
}
