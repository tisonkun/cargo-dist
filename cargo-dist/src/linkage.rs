//! The Linkage Checker, which lets us detect what a binary dynamically links to (and why)

use std::{
    fs::{self, File},
    io::{Cursor, Read},
};

use axoasset::SourceFile;
use axoprocess::Cmd;
use camino::Utf8PathBuf;
use cargo_dist_schema::{DistManifest, Library, Linkage};
use comfy_table::{presets::UTF8_FULL, Table};
use goblin::Object;
use mach_object::{LoadCommand, OFile};

use crate::{config::Config, errors::*, gather_work, Artifact, DistGraph};

/// Arguments for `cargo dist linkage` ([`do_linkage][])
#[derive(Debug)]
pub struct LinkageArgs {
    /// Print human-readable output
    pub print_output: bool,
    /// Print output as JSON
    pub print_json: bool,
    /// Read linkage data from JSON rather than performing a live check
    pub from_json: Option<String>,
}

/// Determinage dynamic linkage of built artifacts (impl of `cargo dist linkage`)
pub fn do_linkage(cfg: &Config, args: &LinkageArgs) -> Result<()> {
    let (dist, _manifest) = gather_work(cfg)?;

    let reports: Vec<Linkage> = if let Some(target) = args.from_json.clone() {
        let file = SourceFile::load_local(target)?;
        file.deserialize_json()?
    } else {
        fetch_linkage(cfg.targets.clone(), dist.artifacts, dist.dist_dir)?
    };

    if args.print_output {
        for report in &reports {
            eprintln!("{}", report_linkage(report));
        }
    }
    if args.print_json {
        let j = serde_json::to_string(&reports).unwrap();
        println!("{}", j);
    }

    Ok(())
}

/// Compute the linkage of local builds and add them to the DistManifest
pub fn add_linkage_to_manifest(
    cfg: &Config,
    dist: &DistGraph,
    manifest: &mut DistManifest,
) -> Result<()> {
    let linkage = fetch_linkage(
        cfg.targets.clone(),
        dist.artifacts.clone(),
        dist.dist_dir.clone(),
    )?;

    manifest.linkage.extend(linkage);
    Ok(())
}

fn fetch_linkage(
    targets: Vec<String>,
    artifacts: Vec<Artifact>,
    dist_dir: Utf8PathBuf,
) -> DistResult<Vec<Linkage>> {
    let mut reports = vec![];

    for target in targets {
        let artifacts: Vec<Artifact> = artifacts
            .clone()
            .into_iter()
            .filter(|r| r.target_triples.contains(&target))
            .collect();

        if artifacts.is_empty() {
            eprintln!("No matching artifact for target {target}");
            continue;
        }

        for artifact in artifacts {
            let path = Utf8PathBuf::from(&dist_dir).join(format!("{}-{target}", artifact.id));

            for (_, binary) in artifact.required_binaries {
                let bin_path = path.join(binary);
                if !bin_path.exists() {
                    eprintln!("Binary {bin_path} missing; skipping check");
                } else {
                    reports.push(determine_linkage(&bin_path, &target)?);
                }
            }
        }
    }

    Ok(reports)
}

/// Formatted human-readable output
pub fn report_linkage(linkage: &Linkage) -> String {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_header(vec!["Category", "Libraries"])
        .add_row(vec![
            "System",
            linkage
                .system
                .clone()
                .into_iter()
                .map(|l| l.to_string())
                .collect::<Vec<String>>()
                .join("\n")
                .as_str(),
        ])
        .add_row(vec![
            "Homebrew",
            linkage
                .homebrew
                .clone()
                .into_iter()
                .map(|l| l.to_string())
                .collect::<Vec<String>>()
                .join("\n")
                .as_str(),
        ])
        .add_row(vec![
            "Public (unmanaged)",
            linkage
                .public_unmanaged
                .clone()
                .into_iter()
                .map(|l| l.path)
                .collect::<Vec<String>>()
                .join("\n")
                .as_str(),
        ])
        .add_row(vec![
            "Frameworks",
            linkage
                .frameworks
                .clone()
                .into_iter()
                .map(|l| l.path)
                .collect::<Vec<String>>()
                .join("\n")
                .as_str(),
        ])
        .add_row(vec![
            "Other",
            linkage
                .other
                .clone()
                .into_iter()
                .map(|l| l.to_string())
                .collect::<Vec<String>>()
                .join("\n")
                .as_str(),
        ]);

    use std::fmt::Write;
    let mut output = String::new();
    if let (Some(bin), Some(target)) = (&linkage.binary, &linkage.target) {
        writeln!(&mut output, "{} ({}):\n", bin, target).unwrap();
    }
    write!(&mut output, "{table}").unwrap();
    output
}

/// Create a homebrew library for the given path
pub fn library_from_homebrew(library: String) -> Library {
    // Doesn't currently support Homebrew installations in
    // non-default locations
    let brew_prefix = if library.starts_with("/opt/homebrew/opt/") {
        Some("/opt/homebrew/opt/")
    } else if library.starts_with("/usr/local/opt/") {
        Some("/usr/local/opt/")
    } else {
        None
    };

    if let Some(prefix) = brew_prefix {
        let cloned = library.clone();
        let stripped = cloned.strip_prefix(prefix).unwrap();
        let mut package = stripped.split('/').next().unwrap().to_owned();

        // The path alone isn't enough to determine the tap the formula
        // came from. If the install receipt exists, we can use it to
        // get the name of the source tap.
        let receipt = Utf8PathBuf::from(&prefix)
            .join(&package)
            .join("INSTALL_RECEIPT.json");

        // If the receipt doesn't exist or can't be loaded, that's not an
        // error; we can fall back to the package basename we parsed out
        // of the path.
        if receipt.exists() {
            let _ = SourceFile::load_local(&receipt)
                .and_then(|file| file.deserialize_json())
                .map(|parsed: serde_json::Value| {
                    if let Some(tap) = parsed["source"]["tap"].as_str() {
                        if tap != "homebrew/core" {
                            package = format!("{tap}/{package}");
                        }
                    }
                });
        }

        Library {
            path: library,
            source: Some(package.to_owned()),
        }
    } else {
        Library {
            path: library,
            source: None,
        }
    }
}

/// Create an apt library for the given path
pub fn library_from_apt(library: String) -> DistResult<Library> {
    // We can't get this information on other OSs
    if std::env::consts::OS != "linux" {
        return Ok(Library {
            path: library,
            source: None,
        });
    }

    let process = Cmd::new("dpkg", "get linkage info from dpkg")
        .arg("--search")
        .arg(&library)
        .output();
    match process {
        Ok(output) => {
            let output = String::from_utf8(output.stdout)?;

            let package = output.split(':').next().unwrap();
            let source = if package.is_empty() {
                None
            } else {
                Some(package.to_owned())
            };

            Ok(Library {
                path: library,
                source,
            })
        }
        // Couldn't find a package for this file
        Err(_) => Ok(Library {
            path: library,
            source: None,
        }),
    }
}

fn do_otool(path: &Utf8PathBuf) -> DistResult<Vec<String>> {
    let mut libraries = vec![];

    let mut f = File::open(path)?;
    let mut buf = vec![];
    let size = f.read_to_end(&mut buf).unwrap();
    let mut cur = Cursor::new(&buf[..size]);
    if let Ok(OFile::MachFile {
        header: _,
        commands,
    }) = OFile::parse(&mut cur)
    {
        let commands = commands
            .iter()
            .map(|load| load.command())
            .cloned()
            .collect::<Vec<LoadCommand>>();

        for command in commands {
            match command {
                LoadCommand::IdDyLib(ref dylib)
                | LoadCommand::LoadDyLib(ref dylib)
                | LoadCommand::LoadWeakDyLib(ref dylib)
                | LoadCommand::ReexportDyLib(ref dylib)
                | LoadCommand::LoadUpwardDylib(ref dylib)
                | LoadCommand::LazyLoadDylib(ref dylib) => {
                    libraries.push(dylib.name.to_string());
                }
                _ => {}
            }
        }
    }

    Ok(libraries)
}

fn do_ldd(path: &Utf8PathBuf) -> DistResult<Vec<String>> {
    let mut libraries = vec![];

    // We ignore the status here because for whatever reason arm64 glibc ldd can decide
    // to return non-zero status on binaries with no dynamic linkage (e.g. musl-static).
    // This was observed both in arm64 ubuntu and asahi (both glibc ldd).
    // x64 glibc ldd is perfectly fine with this and returns 0, so... *shrug* compilers!
    let output = Cmd::new("ldd", "get linkage info from ldd")
        .arg(path)
        .check(false)
        .output()?;

    let result = String::from_utf8_lossy(&output.stdout).to_string();
    let lines = result.trim_end().split('\n');

    for line in lines {
        let line = line.trim();

        // There's no dynamic linkage at all; we can safely break,
        // there will be nothing useful to us here.
        if line.starts_with("not a dynamic executable") || line.starts_with("statically linked") {
            break;
        }

        // Not a library that actually concerns us
        if line.starts_with("linux-vdso") {
            continue;
        }

        // Format: libname.so.1 => /path/to/libname.so.1 (address)
        if let Some(path) = line.split(" => ").nth(1) {
            // This may be a symlink rather than the actual underlying library;
            // we resolve the symlink here so that we return the real paths,
            // making it easier to map them to their packages later.
            let lib = (path.split(' ').next().unwrap()).to_owned();
            let realpath = fs::canonicalize(&lib)?;
            libraries.push(realpath.to_string_lossy().to_string());
        } else {
            continue;
        }
    }

    Ok(libraries)
}

fn do_pe(path: &Utf8PathBuf) -> DistResult<Vec<String>> {
    let buf = std::fs::read(path)?;
    match Object::parse(&buf)? {
        Object::PE(pe) => Ok(pe.libraries.into_iter().map(|s| s.to_owned()).collect()),
        _ => Err(DistError::LinkageCheckUnsupportedBinary {}),
    }
}

/// Get the linkage for a single binary
pub fn determine_linkage(path: &Utf8PathBuf, target: &str) -> DistResult<Linkage> {
    let libraries = match target {
        // Can be run on any OS
        "i686-apple-darwin" | "x86_64-apple-darwin" | "aarch64-apple-darwin" => do_otool(path)?,
        "i686-unknown-linux-gnu"
        | "x86_64-unknown-linux-gnu"
        | "aarch64-unknown-linux-gnu"
        | "i686-unknown-linux-musl"
        | "x86_64-unknown-linux-musl"
        | "aarch64-unknown-linux-musl" => {
            // Currently can only be run on Linux
            if std::env::consts::OS != "linux" {
                return Err(DistError::LinkageCheckInvalidOS {
                    host: std::env::consts::OS.to_owned(),
                    target: target.to_owned(),
                });
            }
            do_ldd(path)?
        }
        // Can be run on any OS
        "i686-pc-windows-msvc" | "x86_64-pc-windows-msvc" | "aarch64-pc-windows-msvc" => {
            do_pe(path)?
        }
        _ => return Err(DistError::LinkageCheckUnsupportedBinary {}),
    };

    let mut linkage = Linkage {
        binary: Some(path.file_name().unwrap().to_owned()),
        target: Some(target.to_owned()),
        system: Default::default(),
        homebrew: Default::default(),
        public_unmanaged: Default::default(),
        frameworks: Default::default(),
        other: Default::default(),
    };
    for library in libraries {
        if library.starts_with("/opt/homebrew") {
            linkage
                .homebrew
                .insert(library_from_homebrew(library.clone()));
        } else if library.starts_with("/usr/lib") || library.starts_with("/lib") {
            linkage.system.insert(library_from_apt(library.clone())?);
        } else if library.starts_with("/System/Library/Frameworks")
            || library.starts_with("/Library/Frameworks")
        {
            linkage.frameworks.insert(Library::new(library.clone()));
        } else if library.starts_with("/usr/local") {
            if std::fs::canonicalize(&library)?.starts_with("/usr/local/Cellar") {
                linkage
                    .homebrew
                    .insert(library_from_homebrew(library.clone()));
            } else {
                linkage
                    .public_unmanaged
                    .insert(Library::new(library.clone()));
            }
        } else {
            linkage.other.insert(library_from_apt(library.clone())?);
        }
    }

    Ok(linkage)
}
