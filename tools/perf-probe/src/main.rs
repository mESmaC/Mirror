use perf_probe::frame::{self, Rect, invalid};
use perf_probe::rfb::Client;
use perf_probe::timing;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

type Result<T = ()> = io::Result<T>;

fn main() {
    if let Err(error) = run() {
        eprintln!("perf-probe: {error}");
        std::process::exit(1);
    }
}

struct Args(BTreeMap<String, String>);
impl Args {
    fn parse(values: impl Iterator<Item = String>) -> Result<Self> {
        let mut result = BTreeMap::new();
        let mut values = values;
        while let Some(key) = values.next() {
            if !key.starts_with("--") {
                return Err(invalid("expected --option value pairs"));
            }
            let value = values
                .next()
                .ok_or_else(|| invalid("missing option value"))?;
            if result.insert(key, value).is_some() {
                return Err(invalid("duplicate option"));
            }
        }
        Ok(Self(result))
    }
    fn optional(&mut self, key: &str) -> Option<String> {
        self.0.remove(key)
    }
    fn required(&mut self, key: &str) -> Result<String> {
        self.optional(key)
            .ok_or_else(|| invalid(&format!("missing {key}")))
    }
    fn value(&mut self, key: &str, default: &str) -> String {
        self.optional(key).unwrap_or_else(|| default.to_owned())
    }
    fn number(&mut self, key: &str, default: u64, min: u64, max: u64) -> Result<u64> {
        let value = self
            .optional(key)
            .map(|s| s.parse::<u64>().map_err(|_| invalid(key)))
            .transpose()?
            .unwrap_or(default);
        if !(min..=max).contains(&value) {
            return Err(invalid(&format!("{key} must be in {min}..={max}")));
        }
        Ok(value)
    }
    fn finish(self) -> Result {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(invalid(&format!("unknown options: {:?}", self.0.keys())))
        }
    }
}

struct Vnc {
    address: SocketAddr,
    vmware: bool,
    os: String,
    serial: Option<PathBuf>,
}
impl Vnc {
    fn parse(args: &mut Args) -> Result<Self> {
        let address: SocketAddr = args
            .required("--address")?
            .parse()
            .map_err(|_| invalid("invalid --address"))?;
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(invalid("VNC requires a loopback address and nonzero port"));
        }
        let hypervisor = args.value("--hypervisor", "vmware");
        if !matches!(hypervisor.as_str(), "vmware" | "qemu") {
            return Err(invalid("invalid --hypervisor"));
        }
        let os = args.required("--os")?;
        if !matches!(os.as_str(), "mirror" | "windows" | "ubuntu") {
            return Err(invalid("invalid --os"));
        }
        Ok(Self {
            address,
            vmware: hypervisor == "vmware",
            os,
            serial: args.optional("--serial-log").map(PathBuf::from),
        })
    }
    fn connect(&self, timeout: Duration) -> Result<Client> {
        Client::connect(self.address, timeout)
    }
    fn desktop(&self, client: &Client) -> Result {
        if self.vmware && (client.frame().width, client.frame().height) != (1920, 1080) {
            return Err(invalid("VMware desktop must be 1920x1080 (0018-D3)"));
        }
        Ok(())
    }
    fn pointer_ready(&self) -> Result {
        if self.os != "mirror" {
            return Ok(());
        }
        let path = self
            .serial
            .as_ref()
            .ok_or_else(|| invalid("Mirror pointer events require --serial-log"))?;
        let mut text = String::new();
        File::open(path)?
            .take(16 * 1024 * 1024 + 1)
            .read_to_string(&mut text)?;
        if text.len() > 16 * 1024 * 1024 {
            return Err(invalid("serial log exceeds bound"));
        }
        if !text.lines().any(|line| {
            line.split_whitespace()
                .take(2)
                .eq(["MIRROR_VMMOUSE_READY", "mode=absolute"])
        }) {
            return Err(invalid(
                "missing MIRROR_VMMOUSE_READY mode=absolute before pointer input",
            ));
        }
        Ok(())
    }
}

struct Csv(File);
impl Csv {
    fn new(path: &Path, method: &str) -> Result<Self> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        writeln!(
            file,
            "# schema=mirror.perf/1\n# method={method}\n# run_state=started"
        )?;
        Ok(Self(file))
    }
    fn vnc(&mut self, client: &Client, vnc: &Vnc) -> Result {
        let (width, height) = client.initial_size();
        writeln!(
            self.0,
            "# server_init_width={width}\n# server_init_height={height}\n# os={}\n# hypervisor={}\n# qpc_frequency={}",
            vnc.os,
            if vnc.vmware { "vmware" } else { "qemu" },
            timing::counter()?.1
        )
    }
    fn finish(mut self) -> Result {
        writeln!(self.0, "# run_state=complete")?;
        self.0.sync_all()
    }
}

fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "operation deadline elapsed"))
}

fn full_frame(client: &mut Client, timeout: Duration) -> Result<(u64, u64)> {
    let deadline = Instant::now() + timeout;
    let (start, frequency) = timing::counter()?;
    client.request_update(false, remaining(deadline)?)?;
    loop {
        let update = client.read_update(remaining(deadline)?)?;
        let end = timing::counter()?.0;
        if update.complete {
            return Ok((end, timing::elapsed_ns(start, end, frequency)?));
        }
        client.request_update(update.size_changes.is_empty(), remaining(deadline)?)?;
    }
}

fn masks(args: &mut Args) -> Result<Vec<Rect>> {
    let text = args.optional("--masks").unwrap_or_default();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let masks = text
        .split(';')
        .map(str::parse)
        .collect::<Result<Vec<Rect>>>()?;
    if masks.len() > 256 {
        return Err(invalid("at most 256 masks allowed"));
    }
    Ok(masks)
}

fn position(text: &str) -> Result<(u16, u16)> {
    let (x, y) = text
        .split_once(',')
        .ok_or_else(|| invalid("position must be x,y"))?;
    Ok((
        x.parse().map_err(|_| invalid("invalid x"))?,
        y.parse().map_err(|_| invalid("invalid y"))?,
    ))
}

fn snapshot(mut args: Args) -> Result {
    let vnc = Vnc::parse(&mut args)?;
    let output = PathBuf::from(args.required("--output")?);
    let csv_path = PathBuf::from(args.required("--csv")?);
    let masks = masks(&mut args)?;
    let pointer = args
        .optional("--pointer")
        .map(|s| position(&s))
        .transpose()?;
    let mut targets = Vec::new();
    for key in ["--launcher", "--document", "--close"] {
        if let Some(text) = args.optional(key) {
            targets.push((key, position(&text)?));
        }
    }
    let response = args
        .optional("--response-rect")
        .map(|s| s.parse::<Rect>())
        .transpose()?;
    args.finish()?;
    let mut client = vnc.connect(Duration::from_secs(3))?;
    vnc.desktop(&client)?;
    if let Some((x, y)) = pointer {
        vnc.pointer_ready()?;
        client.pointer_event(x, y, 0, Duration::from_millis(100))?;
    }
    full_frame(&mut client, Duration::from_secs(10))?;
    vnc.desktop(&client)?;
    for mask in &masks {
        mask.validate(client.frame())?;
    }
    if let Some(rect) = response {
        rect.validate(client.frame())?;
    }
    for (_, (x, y)) in &targets {
        if *x >= client.frame().width || *y >= client.frame().height {
            return Err(invalid("target outside framebuffer"));
        }
    }
    let mut csv = Csv::new(&csv_path, "snapshot")?;
    csv.vnc(&client, &vnc)?;
    for mask in masks {
        writeln!(csv.0, "# mask={mask}")?;
    }
    for (key, (x, y)) in targets {
        writeln!(csv.0, "# {}={x},{y}", &key[2..])?;
    }
    if let Some(rect) = response {
        writeln!(csv.0, "# response_rect={rect}")?;
    }
    frame::write_ppm(&output, client.frame())?;
    csv.finish()
}

fn read_start(path: &Path, deadline: Instant) -> Result<(u64, u64)> {
    loop {
        remaining(deadline)?;
        match File::open(path) {
            Ok(file) => {
                let mut text = String::new();
                file.take(129).read_to_string(&mut text)?;
                if text.len() > 128 {
                    return Err(invalid("oversized QPC start record"));
                }
                let fields = text
                    .split_whitespace()
                    .map(str::parse::<u64>)
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|_| invalid("invalid QPC start record"))?;
                if fields.len() != 2 || fields[1] != timing::counter()?.1 {
                    return Err(invalid(
                        "QPC start requires ticks and this host's frequency",
                    ));
                }
                return Ok((fields[0], fields[1]));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => return Err(error),
        }
    }
}

fn boot(mut args: Args) -> Result {
    let vnc = Vnc::parse(&mut args)?;
    let reference = frame::read_ppm(Path::new(&args.required("--reference")?))?;
    if vnc.vmware && (reference.width, reference.height) != (1920, 1080) {
        return Err(invalid("VMware boot reference must be 1920x1080"));
    }
    let start_file = PathBuf::from(args.required("--start-file")?);
    let output = PathBuf::from(args.required("--output")?);
    let end_frame = PathBuf::from(args.required("--end-frame")?);
    let masks = masks(&mut args)?;
    // There is deliberately no invented default for D5's unconfirmed threshold.
    let max_changed: u64 = args
        .required("--max-changed")?
        .parse()
        .map_err(|_| invalid("invalid --max-changed"))?;
    let tolerance = args.number("--channel-tolerance", 0, 0, 255)? as u8;
    let timeout = Duration::from_secs(args.number("--timeout-seconds", 180, 1, 1800)?);
    let phase = args.value("--phase", "measured");
    if !matches!(phase.as_str(), "warmup" | "measured") {
        return Err(invalid("invalid phase"));
    }
    args.finish()?;
    let pixels = frame::difference(
        &reference,
        &reference,
        frame::full_rect(&reference),
        &masks,
        tolerance,
    )?
    .compared;
    if max_changed >= pixels {
        return Err(invalid(
            "boot threshold must leave at least one required matching pixel",
        ));
    }
    let deadline = Instant::now() + timeout;
    let (start, frequency) = read_start(&start_file, deadline)?;
    let mut csv = Csv::new(&output, "boot-masked-frame")?;
    writeln!(
        csv.0,
        "# max_changed={max_changed}\n# channel_tolerance={tolerance}"
    )?;
    for mask in &masks {
        writeln!(csv.0, "# mask={mask}")?;
    }
    writeln!(
        csv.0,
        "phase,status,duration_ns,qpc_start,qpc_end,changed_pixels"
    )?;
    let mut client = loop {
        let budget = remaining(deadline)?;
        match vnc.connect(budget.min(Duration::from_secs(1))) {
            Ok(client) => break client,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                ) =>
            {
                std::thread::sleep(Duration::from_millis(25))
            }
            Err(error) => return Err(error),
        }
    };
    csv.vnc(&client, &vnc)?;
    client.request_update(false, remaining(deadline)?)?;
    loop {
        let update = match client.read_update(remaining(deadline)?) {
            Ok(update) => update,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                writeln!(csv.0, "{phase},timeout,,{start},,")?;
                csv.finish()?;
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        let end = timing::counter()?.0;
        for (width, height) in &update.size_changes {
            writeln!(csv.0, "# desktop_size={width},{height}")?;
        }
        if update.complete
            && (client.frame().width, client.frame().height) == (reference.width, reference.height)
        {
            let difference = frame::difference(
                &reference,
                client.frame(),
                frame::full_rect(&reference),
                &masks,
                tolerance,
            )?;
            if difference.changed <= max_changed {
                writeln!(
                    csv.0,
                    "{phase},ok,{},{start},{end},{}",
                    timing::elapsed_ns(start, end, frequency)?,
                    difference.changed
                )?;
                frame::write_ppm(&end_frame, client.frame())?;
                return csv.finish();
            }
        }
        client.request_update(update.size_changes.is_empty(), remaining(deadline)?)?;
    }
}

fn run() -> Result {
    let mut values = std::env::args().skip(1);
    let command = values.next().unwrap_or_else(|| "help".to_owned());
    if matches!(command.as_str(), "help" | "--help") {
        println!(
            "perf-probe: snapshot, boot (see tools/perf-probe/README.md). All options use --name value; artifacts must not exist."
        );
        return Ok(());
    }
    let args = Args::parse(values)?;
    match command.as_str() {
        "snapshot" => snapshot(args),
        "boot" => boot(args),
        _ => Err(invalid("unknown subcommand")),
    }
}
