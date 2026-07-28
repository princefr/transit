//! CLI: download BAN départements and build `index.bin`.
//!
//! Usage:
//!   ban-index --data-dir ./data/ban --download --build
//!   ban-index --data-dir ./data/ban --dept 75,92 --download --build
//!   ban-index --data-dir ./data/ban --suggest "12 rue de rivoli"

use ban_search::{download_departments_force, BanConfig, BanIndex, IDF_DEPARTMENTS};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!(
            "ban-index — Base Adresse Nationale\n\n\
             --data-dir DIR     default ./data/ban\n\
             --dept LIST        comma-separated (default IDF: {})\n\
             --download         fetch CSV.gz from adresse.data.gouv.fr\n\
             --force            re-download even if CSV exists\n\
             --build            build index.bin from CSV\n\
             --suggest QUERY    run suggest against index.bin\n",
            IDF_DEPARTMENTS.join(",")
        );
        return ExitCode::SUCCESS;
    }

    let mut data_dir = PathBuf::from("./data/ban");
    let mut depts: Option<Vec<String>> = None;
    let mut do_download = false;
    let mut force = false;
    let mut do_build = false;
    let mut suggest: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--data-dir" => {
                i += 1;
                data_dir = PathBuf::from(args.get(i).expect("--data-dir value"));
            }
            "--dept" => {
                i += 1;
                depts = Some(
                    args.get(i)
                        .expect("--dept value")
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                );
            }
            "--download" => do_download = true,
            "--force" => force = true,
            "--build" => do_build = true,
            "--suggest" => {
                i += 1;
                suggest = Some(args.get(i).expect("--suggest value").clone());
            }
            other => {
                eprintln!("unknown arg: {other}");
                return ExitCode::FAILURE;
            }
        }
        i += 1;
    }

    let mut cfg = BanConfig::idf_default(&data_dir);
    if let Some(d) = depts {
        cfg.departments = d;
    }

    if let Err(e) = cfg.ensure_dirs() {
        eprintln!("mkdir: {e}");
        return ExitCode::FAILURE;
    }

    if do_download {
        match download_departments_force(&cfg, force) {
            Ok(paths) => {
                for p in paths {
                    println!("csv {}", p.display());
                }
            }
            Err(e) => {
                eprintln!("download failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    if do_build {
        match BanIndex::build_from_csv_dir(&cfg.csv_dir(), &cfg.departments) {
            Ok(idx) => {
                println!(
                    "built streets={} addresses={}",
                    idx.street_count(),
                    idx.address_count()
                );
                if let Err(e) = idx.save(&cfg.index_path()) {
                    eprintln!("save failed: {e}");
                    return ExitCode::FAILURE;
                }
                println!("saved {}", cfg.index_path().display());
            }
            Err(e) => {
                eprintln!("build failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    if let Some(ref q) = suggest {
        let idx = match BanIndex::load(&cfg.index_path()) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("load index: {e}");
                return ExitCode::FAILURE;
            }
        };
        for h in idx.suggest(q, 10) {
            println!(
                "{:.1}  {:.5},{:.5}  [{}] {}",
                h.score, h.lat, h.lon, h.kind, h.label
            );
        }
    }

    if !do_download && !do_build && suggest.is_none() {
        eprintln!("nothing to do — pass --download, --build, and/or --suggest");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}
