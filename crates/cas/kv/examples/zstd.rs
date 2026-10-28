//! Run with `cargo run -p covalence-cas-kv --example zstd`.

use covalence_cas_kv::{Compression, KvCas};
use covalence_cas_trait::{Get, Put};
use covalence_kv_memory::MemoryKv;
use covalence_kv_trait::Kv;
use covalence_lib_cid::Cid;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let kv = MemoryKv::default().into_dyn();
    let raw = KvCas::with_prefix(kv.clone(), "example")?;
    let compressed =
        KvCas::with_prefix(kv.clone(), "example")?.with_compression(Compression::zstd(3));
    let content =
        "Content addresses describe logical bytes, independently of storage.\n".repeat(1024);

    let raw_cid: Cid = raw.put(content.clone().into())?;
    let compressed_cid: Cid = compressed.put(content.clone().into())?;
    assert_eq!(raw_cid, compressed_cid);
    assert_eq!(
        compressed.get(&compressed_cid)?.unwrap(),
        content.as_bytes()
    );

    let digest = data_encoding::BASE32_NOPAD
        .encode(compressed_cid.digest())
        .to_ascii_lowercase();
    let key = format!("example/blake3/{digest}/bytes");
    let raw_size = kv.size(key.as_bytes())?.unwrap();
    let compressed_size = kv.size(format!("{key}.zst").as_bytes())?.unwrap();
    println!("CID (both stores): {compressed_cid}");
    println!("Logical bytes: {}", content.len());
    println!("Stored raw: {raw_size} bytes");
    println!("Stored zstd: {compressed_size} bytes");
    println!("Decoded bytes match: true");
    Ok(())
}
