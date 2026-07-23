//! Dumps an FST built by the Rust FstCompiler to /tmp/fst-cross.bin in the
//! exact layout of Java's FST.save(Path) ([FSTMetadata][FST bytes]), plus a
//! plain-text manifest of the (input -> output) mapping for the Java
//! read-back verifier (interop/java/FstReadVerify.java).

use std::io::Write;

use codec_lucene9::FSDirectory;
use codec_lucene9::fst::FstCompiler;

fn main() -> std::io::Result<()> {
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = vec![
        (b"".to_vec(), vec![0x01]),
        (b"ab".to_vec(), vec![0xAA, 0xBB]),
        (b"abc".to_vec(), vec![0xAA, 0xBB, 0xCC]),
        (b"abd".to_vec(), vec![0x01, 0x02, 0x03, 0x04]),
        (b"b".to_vec(), vec![0xFF]),
        (b"zzzz".to_vec(), vec![0x10, 0x20]),
    ];
    // A larger deterministic set sharing long prefixes.
    for i in 0..500u32 {
        entries.push((
            format!("term{:04}", i).into_bytes(),
            vec![(i >> 8) as u8, i as u8, 0x5A],
        ));
    }
    entries.sort();

    let mut compiler = FstCompiler::new();
    for (input, output) in &entries {
        compiler.add(input, Some(output));
    }
    let fst = compiler.finish();

    let dir = FSDirectory::open("/tmp")?;
    let mut out = dir.create_output("fst-cross.bin")?;
    fst.write_metadata(&mut out)?;
    out.write_bytes(fst.bytes())?;
    out.flush()?;

    let mut manifest = std::fs::File::create("/tmp/fst-cross.txt")?;
    for (input, output) in &entries {
        let esc = |b: &[u8]| b.iter().map(|x| format!("{:02x}", x)).collect::<String>();
        writeln!(manifest, "{}\t{}", esc(input), esc(output))?;
    }
    println!(
        "wrote {} entries, fst num_bytes={}",
        entries.len(),
        fst.num_bytes()
    );
    Ok(())
}
