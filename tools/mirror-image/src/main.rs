#![forbid(unsafe_code)]
//! `mirror-image disk --esp <dir> --out <img> --vmdk <vmdk>` writes Mirror's
//! disk image and VMDK descriptor; `mirror-image verify --esp <dir> <img>`
//! reads an image back and compares it with the staged ESP (spec 0015a).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

const USAGE: &str = "usage: mirror-image disk --esp <dir> --out <img> --vmdk <vmdk> [--perf]\n       mirror-image verify --esp <dir> <img>";

enum Command {
    Disk {
        esp: PathBuf,
        image: PathBuf,
        vmdk: PathBuf,
        perf: bool,
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
        Command::Disk {
            esp,
            image,
            vmdk,
            perf,
        } => mirror_image::write_disk_with_perf(esp, image, vmdk, *perf).map(|summary| {
            format!(
                "wrote {} ({} bytes; ESP {} files, {} bytes) and {} ({} bytes)",
                image.display(),
                mirror_image::DISK_BYTES,
                summary.esp_files,
                summary.esp_content_bytes,
                vmdk.display(),
                summary.descriptor_bytes
            )
        }),
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
    let mut perf = false;
    let mut positional = Vec::new();
    let mut rest = rest.iter();
    while let Some(argument) = rest.next() {
        let slot = match argument.as_str() {
            "--perf" if command == "disk" => {
                if perf {
                    return Err("--perf given twice".to_string());
                }
                perf = true;
                continue;
            }
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
        if value.is_empty() || value.starts_with("--") {
            return Err(format!("{argument} needs a path value"));
        }
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
            perf,
        }),
        "verify" if positional.len() == 1 => Ok(Command::Verify {
            esp,
            image: positional.remove(0),
        }),
        "disk" | "verify" => Err("wrong number of arguments".to_string()),
        _ => Err(format!("unknown command {command}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(input: &str) -> Vec<String> {
        input.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn perf_is_a_single_valueless_disk_flag() {
        let disk = "disk --esp esp --out mirror.img --vmdk mirror.vmdk";
        assert!(matches!(
            parse(&arguments(disk)),
            Ok(Command::Disk { perf: false, .. })
        ));
        for input in [
            format!("{disk} --perf"),
            format!("disk --perf {}", &disk[5..]),
        ] {
            assert!(matches!(
                parse(&arguments(&input)),
                Ok(Command::Disk { perf: true, .. })
            ));
        }
        for input in [
            format!("{disk} --perf --perf"),
            format!("{disk} --perf true"),
            format!("{disk} --perf=false"),
            "disk --esp --perf --out mirror.img --vmdk mirror.vmdk --perf".to_string(),
            "verify --esp esp mirror.img --perf".to_string(),
        ] {
            assert!(parse(&arguments(&input)).is_err(), "{input}");
        }
    }
}
