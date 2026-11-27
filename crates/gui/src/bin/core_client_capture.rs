//! Screen-diff evidence of the new GUI (`crates/gui/src/core_client/`).
//!
//! ```text
//! core_client_capture capture   --out DIR --font-file PATH [--scenario ID]...
//! core_client_capture compare   --after DIR [--before DIR] --out DIR
//! core_client_capture identical --first DIR --second DIR
//! core_client_capture list
//! core_client_capture texts     --scenario ID
//! ```
//!
//! `capture` draws the fixed scenarios from a synthetic fixture and writes the PNGs and
//! `manifest.json`; `compare` compares two captures pixel by pixel; `identical` demands
//! that two captures of the same code agree. `scripts/core-gui-visual/` starts them.

#[allow(dead_code)]
#[path = "../../visual-support/mod.rs"]
mod visual;

use std::path::{Path, PathBuf};

use sha2::Digest;
use visual::{
    compare::{self, Status},
    environment, font,
    manifest::{self, Capture, EnvironmentRecord, Manifest, ScenarioRecord},
    scenarios,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(error) = run(&args) {
        eprintln!("core_client_capture: {error}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let (command, rest) = args
        .split_first()
        .ok_or("expected a command: capture, compare, identical, list or texts")?;
    let mut options = Options::parse(rest)?;
    match command.as_str() {
        "capture" => capture(&mut options),
        "compare" => compare_command(&mut options),
        "identical" => identical(&mut options),
        "list" => {
            for spec in scenarios::SCENARIOS {
                println!("{}", spec.id);
            }
            Ok(())
        }
        "texts" => texts(&mut options),
        other => Err(format!("unknown command {other:?}")),
    }
}

struct Options {
    values: Vec<(String, String)>,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut values = Vec::new();
        let mut it = args.iter();
        while let Some(flag) = it.next() {
            let name = flag
                .strip_prefix("--")
                .ok_or_else(|| format!("unexpected argument {flag:?}"))?;
            let value = it.next().ok_or_else(|| format!("--{name} needs a value"))?;
            values.push((name.to_owned(), value.clone()));
        }
        Ok(Self { values })
    }

    fn all(&self, name: &str) -> Vec<String> {
        self.values
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn one(&mut self, name: &str) -> Result<Option<String>, String> {
        let mut found = self.all(name);
        match found.len() {
            0 => Ok(None),
            1 => Ok(found.pop()),
            _ => Err(format!("--{name} given more than once")),
        }
    }

    fn required(&mut self, name: &str) -> Result<String, String> {
        self.one(name)?
            .ok_or_else(|| format!("--{name} is required"))
    }
}

fn capture(options: &mut Options) -> Result<(), String> {
    environment::check_process_environment()?;
    let out = PathBuf::from(options.required("out")?);
    let font = font::pinned(&options.required("font-file")?)?;
    let mut ids = options.all("scenario");
    if ids.is_empty() {
        ids = scenarios::SCENARIOS
            .iter()
            .map(|spec| spec.id.to_owned())
            .collect();
    }
    std::fs::create_dir_all(&out)
        .map_err(|error| format!("cannot create {}: {error}", out.display()))?;
    let forbidden = environment::forbidden_texts(&[&out]);
    let mut records = Vec::new();
    for id in &ids {
        let mut built = scenarios::build(id, &forbidden)?;
        let image = built.render()?;
        let file = format!("{id}.png");
        manifest::write_png(&out.join(&file), &image)?;
        records.push(ScenarioRecord {
            id: id.clone(),
            file,
            width: image.width(),
            height: image.height(),
            rgba_sha256: manifest::rgba_sha256(&image),
        });
        eprintln!("captured {id} ({}x{})", image.width(), image.height());
    }
    let manifest = Manifest {
        schema: manifest::SCHEMA,
        fixture_sha256: manifest::hex(&sha2::Sha256::digest(visual::FIXTURE_SOURCE)),
        theme: scenarios::THEME.into(),
        dpi: scenarios::DPI,
        font,
        environment: EnvironmentRecord {
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            wgpu_backend: std::env::var("WGPU_BACKEND").ok(),
        },
        scenarios: records,
    };
    let text = serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?;
    std::fs::write(out.join(manifest::MANIFEST_FILE), text).map_err(|error| error.to_string())
}

fn load(dir: &str) -> Result<Capture, String> {
    manifest::load(Path::new(dir))
}

fn compare_command(options: &mut Options) -> Result<(), String> {
    let after = load(&options.required("after")?)?;
    let before = options.one("before")?.map(|dir| load(&dir)).transpose()?;
    let out = PathBuf::from(options.required("out")?);
    let report = compare::compare(before.as_ref(), &after, &out)?;
    if !report.baseline {
        println!("no baseline: every scenario is new");
    }
    if report.fixture_changed {
        println!("the fixture source changed between the sides");
    }
    for entry in &report.entries {
        let what = match entry.status {
            Status::Unchanged => "unchanged".to_owned(),
            Status::Changed => format!("changed ({} pixels)", entry.changed_pixels),
            Status::Added => "added".to_owned(),
            Status::Removed => "removed".to_owned(),
        };
        println!("{}: {what}", entry.id);
    }
    Ok(())
}

fn identical(options: &mut Options) -> Result<(), String> {
    let first = load(&options.required("first")?)?;
    let second = load(&options.required("second")?)?;
    compare::require_identical(&first, &second)?;
    println!(
        "the two captures are identical ({} scenarios)",
        first.images.len()
    );
    Ok(())
}

fn texts(options: &mut Options) -> Result<(), String> {
    let id = options.required("scenario")?;
    let built = scenarios::build(&id, &[])?;
    for text in built.texts() {
        println!("{text}");
    }
    Ok(())
}
