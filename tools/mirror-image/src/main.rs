#![forbid(unsafe_code)]
//! `mirror-image disk --esp <dir> --out <img> --vmdk <vmdk>` writes Mirror's
//! disk image and VMDK descriptor; `mirror-image verify --esp <dir> <img>`
//! reads an image back and compares it with the staged ESP (spec 0015a).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

const USAGE: &str = "usage: mirror-image disk --esp <dir> --out <img> --vmdk <vmdk>\n       mirror-image verify --esp <dir> <img>";

enum Command {
    Disk {
        esp: PathBuf,
        image: PathBuf,
        vmdk: PathBuf,
    },
    Verify {
        esp: PathBuf,
        image: PathBuf,
    },
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse(&arguments) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("mirror-image: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let start = Instant::now();
    let result = match &command {
        Command::Disk { esp, image, vmdk } => {
            mirror_image::write_disk(esp, image, vmdk).map(|summary| {
                format!(
                    "wrote {} ({} bytes; ESP {} files, {} bytes) and {} ({} bytes)",
                    image.display(),
                    mirror_image::DISK_BYTES,
                    summary.esp_files,
                    summary.esp_content_bytes,
                    vmdk.display(),
                    summary.descriptor_bytes
                )
            })
        }
        Command::Verify { esp, image } => mirror_image::verify(esp, image).map(|summary| {
            format!(
                "verified {}: GPT, ESP ({} files, {} bytes, {} clusters) and ext2 root ({} groups)",
                image.display(),
                summary.esp_files,
                summary.esp_content_bytes,
                summary.esp_clusters,
                summary.root_groups
            )
        }),
    };
    match result {
        Ok(message) => {
            println!(
                "mirror-image: {message} in {:.3} s",
                start.elapsed().as_secs_f64()
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("mirror-image: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Parses the arguments strictly: each option once, nothing unknown.
fn parse(arguments: &[String]) -> Result<Command, String> {
    let (command, rest) = arguments.split_first().ok_or("no command")?;
    let mut esp = None;
    let mut image = None;
    let mut vmdk = None;
    let mut positional = Vec::new();
    let mut rest = rest.iter();
    while let Some(argument) = rest.next() {
        let slot = match argument.as_str() {
            "--esp" => &mut esp,
            "--out" if command == "disk" => &mut image,
            "--vmdk" if command == "disk" => &mut vmdk,
            option if option.starts_with("--") => return Err(format!("unknown option {option}")),
            _ => {
                positional.push(PathBuf::from(argument));
                continue;
            }
        };
        let value = rest
            .next()
            .ok_or_else(|| format!("{argument} needs a value"))?;
        if slot.replace(PathBuf::from(value)).is_some() {
            return Err(format!("{argument} given twice"));
        }
    }
    let esp = esp.ok_or("--esp is required")?;
    match command.as_str() {
        "disk" if positional.is_empty() => Ok(Command::Disk {
            esp,
            image: image.ok_or("--out is required")?,
            vmdk: vmdk.ok_or("--vmdk is required")?,
        }),
        "verify" if positional.len() == 1 => Ok(Command::Verify {
            esp,
            image: positional.remove(0),
        }),
        "disk" | "verify" => Err("wrong number of arguments".to_string()),
        _ => Err(format!("unknown command {command}")),
    }
}
