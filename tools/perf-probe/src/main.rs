use std::{collections::BTreeMap, ffi::OsString, io, path::PathBuf};

#[cfg(windows)]
fn execute(args: Vec<OsString>) -> io::Result<()> {
    use perf_probe::{
        native_package::{NativePackage, Options},
        package::Verdict,
        record,
    };
    let (command, values, release) = parse(args)?;
    match command.as_str() {
        "package" => {}
        "vm" | "vm-recover" => {
            perf_probe::vm::require_live_entry()?;
            return Err(record::invalid("live VMware adapter is unavailable"));
        }
        _ => return Err(record::invalid("unknown command")),
    }
    let required = |key: &str| {
        values
            .get(key)
            .cloned()
            .ok_or_else(|| record::invalid(format!("missing {key}")))
    };
    let checkout = PathBuf::from(required("--checkout")?);
    let cargo_home = values
        .get("--cargo-home")
        .cloned()
        .or_else(|| std::env::var_os("CARGO_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default()).join(".cargo")
        });
    let limine_archive = values
        .get("--limine-archive")
        .map(PathBuf::from)
        .unwrap_or_else(|| checkout.join(".tools/limine/v12.9.3/limine-binary.zip"));
    let string = |v: OsString| {
        v.into_string()
            .map_err(|_| record::invalid("non-Unicode command argument"))
    };
    let options = Options {
        checkout,
        lane_root: required("--lane-root")?.into(),
        operator: string(required("--owner")?)?,
        release,
        recover: values.get("--recover").cloned().map(string).transpose()?,
        slot: values.get("--slot").cloned().map(string).transpose()?,
        cargo_home,
        limine_archive,
    };
    if options.recover.is_some() && release {
        return Err(record::invalid("recovery cannot choose a build profile"));
    }
    let result = NativePackage::open(options)?.execute()?;
    println!("{}", result.verdict.name());
    if result.verdict != Verdict::FinalizedValid {
        return Err(io::Error::other(
            result
                .first_failure
                .unwrap_or_else(|| "package remains invalid".into()),
        ));
    }
    Ok(())
}
#[cfg(not(windows))]
fn execute(_: Vec<OsString>) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "host lane requires Windows",
    ))
}
type Arguments = (String, BTreeMap<String, OsString>, bool);
fn parse(args: Vec<OsString>) -> io::Result<Arguments> {
    let mut args = args.into_iter();
    let command=args.next().and_then(|x|x.into_string().ok()).ok_or_else(||io::Error::other("usage: perf-probe package --checkout PATH --lane-root PATH --owner nick/task [--release | --recover ID]"))?;
    let mut values = BTreeMap::new();
    let mut release = false;
    while let Some(key) = args.next() {
        let key = key
            .into_string()
            .map_err(|_| io::Error::other("invalid argument name"))?;
        if key == "--release" {
            if release {
                return Err(io::Error::other("duplicate --release"));
            }
            release = true;
            continue;
        }
        if ![
            "--checkout",
            "--lane-root",
            "--owner",
            "--recover",
            "--cargo-home",
            "--limine-archive",
            "--slot",
        ]
        .contains(&key.as_str())
        {
            return Err(io::Error::other(format!("unknown option {key}")));
        }
        let value = args
            .next()
            .ok_or_else(|| io::Error::other(format!("missing value for {key}")))?;
        if value.is_empty()
            || value.to_string_lossy().starts_with("--")
            || values.insert(key, value).is_some()
        {
            return Err(io::Error::other("missing or duplicate option"));
        }
    }
    Ok((command, values, release))
}
fn main() {
    if let Err(e) = execute(std::env::args_os().skip(1).collect()) {
        eprintln!("perf-probe: {e}");
        std::process::exit(2);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_ambiguous_options() {
        for a in [
            vec!["package", "--all"],
            vec!["package", "--owner"],
            vec!["package", "--release", "--release"],
            vec!["package", "--owner", "x/y", "--owner", "x/y"],
        ] {
            assert!(parse(a.into_iter().map(OsString::from).collect()).is_err());
        }
    }
}
