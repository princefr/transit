use transit::gtfs::flash_store;
fn main() {
    let path = std::path::Path::new("data/flash/flash-bc78eee77ed8e9575ae8fb5d488e17f9d5a0630e9d53a7be4954132a7e0d81d3.bin");
    let t = std::time::Instant::now();
    match flash_store::load_flash(path) {
        Ok(Some((cells, ap, fp, tt))) => {
            println!("loaded ok in {:.1}s: cells={} walk_n={} patterns={} trips={}",
                t.elapsed().as_secs_f32(), cells, ap.len(), fp.len(), tt.len());
        }
        Ok(None) => println!("load returned None after {:.1}s", t.elapsed().as_secs_f32()),
        Err(e) => println!("load error: {e}"),
    }
}
