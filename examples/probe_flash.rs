use transit::gtfs::flash_store;
/// Usage: probe_flash <flash-*.bin> <pack fingerprint>
fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(path), Some(fingerprint)) = (args.next(), args.next()) else {
        eprintln!("usage: probe_flash <flash-*.bin> <pack fingerprint>");
        std::process::exit(2);
    };
    let path = std::path::Path::new(&path);
    let t = std::time::Instant::now();
    match flash_store::load_flash(path, &fingerprint) {
        Ok(Some((cells, ap, fp, tt))) => {
            println!(
                "loaded ok in {:.1}s: cells={} walk_n={} patterns={} trips={}",
                t.elapsed().as_secs_f32(),
                cells,
                ap.len(),
                fp.len(),
                tt.len()
            );
        }
        Ok(None) => println!("load returned None after {:.1}s", t.elapsed().as_secs_f32()),
        Err(e) => println!("load error: {e}"),
    }
}
