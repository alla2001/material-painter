//! Signs Material Painter release packages, so the in-app updater (src/update.rs) installs
//! only what came from the release workflow.
//!
//!   update-sign keygen <key file>   new key pair: the private key (PKCS#8, hex) goes to the
//!                                   file, the public key (hex) is printed for src/update.rs
//!   update-sign sign <package>...   writes <package>.sig, with the private key (hex) taken
//!                                   from the UPDATE_SIGNING_KEY environment variable
//!
//! A signature is Ed25519 over "material-painter update\n<file name>\n<sha-256 hex>", so it
//! is tied to the package's name (which carries the version) as well as its contents.

use ring::digest::{Context, SHA256};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use std::io::Read;
use std::path::Path;
use std::process::ExitCode;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    (s.len() % 2 == 0).then_some(())?;
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn sha256(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let (mut ctx, mut buf) = (Context::new(&SHA256), vec![0u8; 1 << 20]);
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hex(ctx.finish().as_ref()));
        }
        ctx.update(&buf[..n]);
    }
}

fn run(args: &[String]) -> Result<(), String> {
    match args {
        [cmd, out] if cmd == "keygen" => {
            if Path::new(out).exists() {
                return Err(format!("{out} exists; not overwriting a key."));
            }
            let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).map_err(|_| "key generation failed")?;
            let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|_| "key generation failed")?;
            std::fs::write(out, hex(pkcs8.as_ref())).map_err(|e| format!("{out}: {e}"))?;
            println!("public key: {}", hex(pair.public_key().as_ref()));
            Ok(())
        }
        [cmd, files @ ..] if cmd == "sign" && !files.is_empty() => {
            let key = std::env::var("UPDATE_SIGNING_KEY").map_err(|_| "UPDATE_SIGNING_KEY isn't set.")?;
            let pkcs8 = unhex(&key).ok_or("UPDATE_SIGNING_KEY isn't hex.")?;
            let pair = Ed25519KeyPair::from_pkcs8(&pkcs8).map_err(|_| "UPDATE_SIGNING_KEY isn't an Ed25519 key.")?;
            println!("signing with public key {}", hex(pair.public_key().as_ref()));
            for file in files {
                let path = Path::new(file);
                let name = path.file_name().ok_or_else(|| format!("{file}: no file name"))?.to_string_lossy();
                let digest = sha256(path).map_err(|e| format!("{file}: {e}"))?;
                let sig = pair.sign(format!("material-painter update\n{name}\n{digest}").as_bytes());
                std::fs::write(format!("{file}.sig"), hex(sig.as_ref())).map_err(|e| format!("{file}.sig: {e}"))?;
                println!("signed {name}");
            }
            Ok(())
        }
        _ => Err("usage: update-sign keygen <key file> | update-sign sign <package>...".into()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
