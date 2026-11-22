//! conditions.json: everything about the host, toolchain, artifacts and inputs
//! that is recorded before the first sample is taken.
use super::{
    fixture,
    gate::{Spec, check_conditions, hex, order_string},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{self, Read},
    path::{Path, PathBuf},
    process::Command,
};

pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut block = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut block)?;
        if n == 0 {
            break;
        }
        hash.update(&block[..n]);
    }
    Ok(hex(&hash.finalize()))
}

fn run(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .map(|o| {
            String::from_utf8_lossy(&[o.stdout, o.stderr].concat())
                .trim()
                .to_owned()
        })
        .unwrap_or_default()
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_owned()
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

fn tool(name: &str, version_arg: &str) -> String {
    match which(name) {
        Some(path) => {
            let real = std::fs::canonicalize(&path).unwrap_or(path.clone());
            let version = first_line(&run(&path.display().to_string(), &[version_arg]));
            format!("{} -> {} | {version}", path.display(), real.display())
        }
        None => String::new(),
    }
}

fn closure(exe: &Path) -> io::Result<Value> {
    let dir = exe
        .parent()
        .ok_or_else(|| io::Error::other("artifact has no directory"))?;
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_file() {
            files.push(json!({
                "name": path.file_name().unwrap().to_string_lossy(),
                "size": std::fs::metadata(&path)?.len(),
                "sha256": sha256_file(&path)?,
            }));
        }
    }
    files.sort_by_key(|f| f["name"].as_str().unwrap_or_default().to_owned());
    Ok(Value::Array(files))
}

fn arm(opts: &HashMap<String, String>, key: &str, with_guard: bool) -> io::Result<Value> {
    let get = |name: &str| {
        opts.get(&format!("{key}-{name}"))
            .cloned()
            .unwrap_or_default()
    };
    let exe = PathBuf::from(
        opts.get(&format!("{key}-exe"))
            .ok_or_else(|| io::Error::other(format!("--{key}-exe")))?,
    );
    let mut value = json!({
        "exe_path": exe.display().to_string(),
        "exe_sha256": sha256_file(&exe)?,
        "size": std::fs::metadata(&exe)?.len(),
        "source_sha": get("sha"),
        "tree_sha": get("tree"),
        "build_recipe": get("recipe"),
        "note": get("note"),
        "closure": closure(&exe)?,
    });
    if with_guard {
        let dir = exe.parent().unwrap();
        value["helper_sha256"] = sha256_file(&dir.join("evorch-process-guard"))?.into();
        value["bundle_sha256"] = sha256_file(&dir.join("guard-bundle.json"))?.into();
    }
    Ok(value)
}

/// Builds conditions.json from the command line and the host.
pub fn collect(opts: &HashMap<String, String>) -> io::Result<Value> {
    let spec = Spec::frozen();
    let scratch = tempfile::tempdir()?;
    let repo_commit = fixture::repository(&scratch.path().join("repo"), &spec.state)?;
    let mut order = serde_json::Map::new();
    for round in 1..=spec.rounds {
        let mut per = serde_json::Map::new();
        for (w, workload) in spec.workloads.iter().enumerate() {
            per.insert(workload.name.clone(), order_string(&spec, round, w).into());
        }
        order.insert(round.to_string(), per.into());
    }
    let meminfo = read("/proc/meminfo");
    let mem = meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.trim().trim_end_matches(" kB").trim().parse::<u64>().ok());
    let cpuinfo = read("/proc/cpuinfo");
    let get = |name: &str| opts.get(name).cloned().unwrap_or_default();
    let runner = std::env::current_exe()?;
    Ok(json!({
        "schema": 1,
        "created_unix": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        "spec_sha256": Spec::sha256(),
        "seed": spec.seed,
        "order": order,
        "host": {
            "uname": run("uname", &["-a"]),
            "kernel": read("/proc/sys/kernel/osrelease"),
            "cpu_model": cpuinfo.lines().find_map(|l| l.strip_prefix("model name")).map(|v| v.trim_start_matches([' ', '\t', ':']).to_owned()).unwrap_or_default(),
            "cpu_count": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
            "governor": read("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
            "mem_total_kib": mem,
            "loadavg": read("/proc/loadavg"),
        },
        "toolchain": {
            "rustc": get("rustc"),
            "cargo": get("cargo"),
            "profile": get("profile"),
            "rustflags": get("rustflags"),
            "rustflags_set": !get("rustflags").is_empty(),
            "libc": run("getconf", &["GNU_LIBC_VERSION"]),
        },
        "tools": {
            "sh": tool("sh", "--version"),
            "grep": tool("grep", "--version"),
            "git": tool("git", "--version"),
            "bwrap": tool("bwrap", "--version"),
        },
        "stress_ng": get("stress-ng"),
        "fixtures": fixture::describe(&spec.state, &repo_commit),
        "runner": {
            "exe_path": runner.display().to_string(),
            "sha256": sha256_file(&runner)?,
            "source_sha": get("runner-sha"),
        },
        "arms": {"a": arm(opts, "a", false)?, "b": arm(opts, "b", true)?},
    }))
}

pub fn problems(value: &Value) -> Vec<String> {
    check_conditions(&Spec::frozen(), value)
}
