use crate::filter::VersionStr;
use aho_corasick::AhoCorasickBuilder;
use anyhow::{anyhow, Result};
use log::{info, warn};
use owo_colors::colored::*;
use rayon::prelude::*;
use regex::Regex;
use reqwest::blocking::Client;
use serde::Serialize;
use std::{
    borrow::Cow,
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use version_compare::{compare_to, Cmp};

mod checker;
mod cli;
mod filter;
mod parser;

const VCS_VERSION_NUMBERS: &[&str] = &["+git", "+hg", "+svn", "+bzr"];

#[derive(Debug)]
struct CheckerResult {
    name: String,
    before: String,
    after: String,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CheckResultOutput {
    name: String,
    before: String,
    after: String,
    path: String,
    warnings: Vec<String>,
}

fn collect_spec(dir: &Path) -> Result<Vec<PathBuf>> {
    let walker = walkdir::WalkDir::new(dir).min_depth(1).max_depth(3);
    let result = walker
        .into_iter()
        .filter_map(|x| {
            let entry = x.ok()?;
            if entry.file_name() == "spec" {
                entry.path().canonicalize().ok()
            } else {
                None
            }
        })
        .collect();

    Ok(result)
}

fn normalize_name(path: &Path) -> Cow<str> {
    let p = path.parent().unwrap_or(path);
    let p = p.file_name().unwrap_or(p.as_os_str());

    p.to_string_lossy()
}

fn update_version<P: AsRef<Path>>(new: &str, spec: P) -> Result<String> {
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(spec.as_ref())?;
    let mut content = String::new();
    f.read_to_string(&mut content)?;
    let replace = Regex::new("VER=.+").unwrap();
    let replace_rel = Regex::new("REL=.+\\s+").unwrap();
    let replaced = replace.replace(&content, format!("VER={}", new));
    let replaced = replace_rel.replace(&replaced, "");
    f.seek(SeekFrom::Start(0))?;
    let bytes = replaced.as_bytes();
    f.write_all(bytes)?;
    f.set_len(bytes.len() as u64)?;

    Ok(replaced.to_string())
}

fn validate_urls(a: &HashMap<String, String>, b: &HashMap<String, String>) -> bool {
    for (key, value) in a.iter() {
        if !key.starts_with("SRCS") {
            continue;
        }
        if let Some(other) = b.get(key) {
            let a_split = value.split_ascii_whitespace();
            let b_split = other.split_ascii_whitespace();
            for (old, new) in a_split.zip(b_split) {
                if old == new {
                    return true;
                }
            }
        }
    }

    false
}

fn check_update_worker<P: AsRef<Path>>(
    client: &Client,
    spec: P,
    dry_run: bool,
    comply: bool,
) -> Result<CheckerResult> {
    let s = parser::parse_spec(spec.as_ref())?;
    let current_version = s
        .get("VER")
        .ok_or_else(|| anyhow!("{}: 'VER' field is missing!", spec.as_ref().display()))?;
    let current_version = current_version.trim();
    let config_line = s.get("CHKUPDATE").ok_or_else(|| {
        anyhow!(
            "{}: 'CHKUPDATE' field is missing, cannot continue!",
            spec.as_ref().display()
        )
    })?;
    let mut warnings = Vec::new();
    let config_line = config_line.to_owned() + ";"; // compensate for the parser quirk
    let config = parser::parse_check_update(&mut config_line.as_str())?;
    let new_version = checker::check_update(&config, client)?;
    let new_version = new_version.trim();
    let new_version = new_version.strip_prefix('v').unwrap_or(new_version);
    let new_version = if comply {
        let new_version_before_modification = new_version;
        let complied = new_version.compily_with_aosc();
        warnings.push(format!(
            "Compliance mode enabled, was '{}'",
            new_version_before_modification
        ));
        complied
    } else {
        new_version.to_string()
    };
    let new_version = new_version.as_str();
    let name = normalize_name(spec.as_ref()).to_string();
    if current_version == new_version {
        return Ok(CheckerResult {
            name,
            warnings,
            before: current_version.to_string(),
            after: new_version.to_string(),
        });
    }
    let snapshot_version = AhoCorasickBuilder::new().build(VCS_VERSION_NUMBERS);
    if current_version.contains('+') && !comply {
        warnings.push(format!("Compound version number '{}'", current_version));
        if let Some(version) = snapshot_version?.find(current_version) {
            warnings.push(format!(
                "Version number indicates a snapshot ({}) is used",
                VCS_VERSION_NUMBERS[version.pattern()]
            ))
        }
    }
    if let Ok(ret) = compare_to(current_version, new_version, Cmp::Gt) {
        if ret {
            warnings.push(format!(
                "Possible downgrade from the current version ({} -> {})",
                current_version, new_version
            ));
        }
    } else {
        warnings.push(format!(
            "Versions not comparable: `{}` and `{}`",
            current_version, new_version
        ));
    }

    if !dry_run {
        let modified = update_version(new_version, spec.as_ref())?;
        let mut new_ctx = HashMap::new();
        match abbs_meta_apml::parse(&modified, &mut new_ctx) {
            Ok(_) => {
                if validate_urls(&s, &new_ctx) {
                    warnings.push("Hardcoded URLs detected.".to_string());
                }
            }
            Err(err) => {
                for i in err {
                    warnings.push(format!("Modified spec is broken: {i}"));
                }
            }
        }
    }

    Ok(CheckerResult {
        name,
        warnings,
        before: current_version.to_string(),
        after: new_version.to_string(),
    })
}

fn print_results(results: &[Result<CheckerResult>], version_only: bool) {
    if version_only {
        for result in results.iter().flatten() {
            println!("{}", result.after);
        }
    } else {
        println!("The following packages were updated:");
        println!("{:<30}{:^44}\t\tIssues", "Name", "Version");
        for result in results.iter().flatten() {
            if result.before == result.after {
                continue;
            }
            println!(
                "{:<30}{:>20} -> {:<20}\t\t{}",
                result.name.cyan(),
                result.before.red(),
                result.after.green(),
                result.warnings.join("; ").yellow()
            );
        }
        println!("\nErrors:");
        for result in results {
            if let Err(e) = result {
                println!("{}", e.bold());
            }
        }
    }
}

fn main() {
    let args = cli::build_cli().get_matches();
    env_logger::init();
    let mut pattern = None;
    if let Some(p) = args.get_one::<String>("INCLUDE") {
        pattern = Some(Regex::new(p).unwrap());
    }
    let dry_run = args.get_flag("DRY_RUN");
    let comply_with_aosc = args.get_flag("COMPLY");
    let version_only = args.get_flag("VERSION_ONLY");
    let update_checksum = args.get_flag("UPDATE_CHECKSUM");
    let current_path = std::env::current_dir().expect("Failed to get current dir.");
    let workdir = if let Some(d) = args.get_one::<String>("DIR") {
        Path::new(d).canonicalize().unwrap()
    } else {
        Path::new(".").canonicalize().unwrap()
    };

    let mut files = if let Some(list) = args.get_one::<String>("FILE") {
        let path = Path::new(list).canonicalize().unwrap();
        std::env::set_current_dir(workdir).expect("Failed to set current directory");
        let list = parser::expand_package_list([&path]);
        list.into_iter()
            .map(|x| Path::new(&x).join("spec"))
            .collect()
    } else {
        std::env::set_current_dir(workdir).expect("Failed to set current directory");
        collect_spec(Path::new(".")).unwrap()
    };

    if let Some(pattern) = pattern {
        files.retain(|x| {
            if let Some(name) = x.parent().map(|p| p.to_string_lossy()) {
                pattern.is_match(&name)
            } else {
                false
            }
        });
    }

    if dry_run {
        warn!("Dry-run mode: files will not be updated.");
    }
    let total = files.len();
    info!("Checking updates for {} packages ...", total);
    let current = Arc::new(AtomicUsize::new(1));

    let results: Vec<_> = files
        .par_iter()
        .map_init(Client::new, |c, f| {
            let name = normalize_name(f);
            let current = current.fetch_add(1, Ordering::SeqCst);
            info!("[{}/{}] Checking {} ...", current, total, &name);
            check_update_worker(c, f, dry_run, comply_with_aosc)
                .map_err(|e| anyhow!("{}: {:?}", name.cyan(), e))
        })
        .collect();

    print_results(&results, version_only);

    if update_checksum {
        // Update checksum via `acbs-build -gw`
        // execute: sudo ciel shell -- acbs-build -gw [packages]
        let mut packages = vec![];
        for result in results.iter().flatten() {
            if result.before == result.after {
                continue;
            }
            packages.push(result.name.as_str());
        }
        let arg = packages.join(" ");

        // add -E to pass CIEL_INST environment variable
        println!(
            "Updating checksum via: sudo -E ciel shell -- acbs-build -gw {}",
            arg
        );
        if !dry_run {
            if let Err(err) = Command::new("sudo")
                .args(["-E", "ciel", "shell", "--", "acbs-build", "-gw", &arg])
                .status()
            {
                println!("Failed with {}", err);
            }
        }
    }

    if let Some(log_file) = args.get_one::<String>("LOG") {
        let path = Path::new(log_file);
        let path = if path.is_absolute() {
            Cow::Borrowed(path)
        } else {
            Cow::Owned(current_path.join(log_file))
        };

        let mut f = File::create(&*path).unwrap();
        let items = results.iter().filter_map(|x| {
            if let Ok(ret) = x {
                Some(ret.name.clone())
            } else {
                None
            }
        });

        let tree = get_tree(Path::new(".")).expect("Can not get tree path!");
        let mut results = vec![];

        for i in items {
            results.push(
                find_path(&i, &tree)
                    .unwrap()
                    .strip_prefix(&tree)
                    .expect("Can not base tree path!")
                    .to_str()
                    .unwrap_or_else(|| panic!("Can not convert path to str: {}", i))
                    .to_string(),
            );
        }

        f.write_all(results.join("\n").as_bytes()).unwrap();
        info!("Wrote results to {}", path.display());
    }

    if let Some(json_file) = args.get_one::<String>("JSON") {
        let path = Path::new(json_file);
        let path = if path.is_absolute() {
            Cow::Borrowed(path)
        } else {
            Cow::Owned(current_path.join(json_file))
        };

        let mut f = File::create(&*path).unwrap();
        let tree = get_tree(Path::new(".")).expect("Can not get tree path!");
        let items = results
            .iter()
            .filter_map(|x| {
                if let Ok(ret) = x {
                    Some(CheckResultOutput {
                        name: ret.name.to_owned(),
                        before: ret.before.to_owned(),
                        after: ret.after.to_owned(),
                        path: find_path(&ret.name, &tree)
                            .unwrap()
                            .strip_prefix(&tree)
                            .expect("Can not base tree path!")
                            .to_str()
                            .unwrap_or_else(|| panic!("Can not convert path to str: {}", ret.name))
                            .to_string(),
                        warnings: ret.warnings.to_vec(),
                    })
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        serde_json::to_writer(&mut f, &items).expect("Failed to write JSON result");
        info!("Wrote results to {}", path.display());
    }
}

fn get_tree(directory: &Path) -> Result<PathBuf> {
    let mut tree = directory.canonicalize()?;
    let mut has_groups;
    loop {
        has_groups = tree.join("groups").is_dir();
        if !has_groups && tree.to_str() == Some("/") {
            return Err(anyhow!("Cannot find ABBS tree!"));
        }
        if has_groups {
            return Ok(tree.to_path_buf());
        }
        tree.pop();
    }
}

fn find_path(name: &str, tree: &Path) -> Result<PathBuf> {
    let packages = walkdir::WalkDir::new(tree).min_depth(2).max_depth(2);

    let mut path = None;

    for entry in packages {
        let entry = entry?;
        let file_name = &entry
            .file_name()
            .to_str()
            .ok_or_else(|| anyhow!("Can not convert str"))?;

        if file_name == &name {
            path = Some(entry.path().to_path_buf());
            break;
        }
    }

    path.ok_or_else(|| anyhow!("Can not get package path: {}", name))
}
