use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use nostr_sdk::Keys;

fn main() {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().expect("usage: gen_nostr_keypair <secret-out-path>"));
    if let Some(parent) = out.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let keys = Keys::generate();
    let secret = keys.secret_key().to_secret_hex();
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&out)
        .expect("write secret");
    writeln!(file, "{secret}").expect("secret line");
    println!("{}", keys.public_key().to_hex());
}
