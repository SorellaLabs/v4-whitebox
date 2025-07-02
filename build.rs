use std::{io::Write, os::unix::process::ExitStatusExt, process::Command};

use convert_case::{Case, Casing};
use itertools::Itertools;

const CONTRACT_LOCATION: &str = "contracts/";
const OUT_DIRECTORY: &str = "contracts/out/";
const SRC_DIRECTORY: &str = "contracts/src/";
const BINDINGS_PATH: &str = "/src/uniswap/loaders/mod.rs";

const WANTED_CONTRACTS: [&str; 2] = ["GetUniswapV4PoolData.sol", "GetUniswapV4TickData.sol"];

// builds the contracts crate. then goes and generates bindings on this
fn main() {
    let base_dir = workspace_dir();

    let binding = base_dir.clone();
    let this_dir = binding.to_str().unwrap();

    let mut contract_dir = base_dir.clone();
    contract_dir.push(CONTRACT_LOCATION);

    // Only rerun if our contracts have actually changed
    let mut src_dir = base_dir.clone();
    src_dir.push(SRC_DIRECTORY);
    if let Some(src_dir_str) = src_dir.to_str() {
        for contract in WANTED_CONTRACTS {
            println!("cargo::rerun-if-changed={src_dir_str}{contract}");
        }
    }

    let mut out_dir = base_dir.clone();
    out_dir.push(OUT_DIRECTORY);

    // Try to find forge in common locations
    let forge_paths = [
        "forge" // Check PATH first
    ];

    let mut forge_cmd = None;
    for path in &forge_paths {
        if Command::new(path).arg("--version").output().is_ok() {
            forge_cmd = Some(path);
            break;
        }
    }

    let forge_path = forge_cmd.unwrap_or_else(|| {
        eprintln!("Error: Foundry (forge) is not installed or not found in PATH.");
        eprintln!(
            "Please install Foundry from: https://book.getfoundry.sh/getting-started/installation"
        );
        eprintln!("Or run: curl -L https://foundry.paradigm.xyz | bash && foundryup");
        panic!("Foundry is required to compile Solidity contracts");
    });

    let res = Command::new(forge_path)
        .env("FOUNDRY_PROFILE", "loaders")
        .arg("build")
        .arg("--optimize")
        .arg("--optimizer-runs")
        .arg("9999999999")
        .current_dir(contract_dir)
        .spawn()
        .expect("Failed to execute forge")
        .wait()
        .unwrap();

    if res.into_raw() != 0 {
        panic!("foundry failed to build files");
    }

    let sol_macro_invocation = std::fs::read_dir(out_dir)
        .unwrap()
        .filter_map(|folder| {
            let folder = folder.ok()?;
            let mut path = folder.path();
            let file_name = path.file_name()?.to_str()?;
            if !WANTED_CONTRACTS.contains(&file_name) {
                return None;
            }
            let raw = file_name.split('.').collect::<Vec<_>>()[0].to_owned();
            path.push(format!("{raw}.json"));

            Some((raw, path.to_str()?.to_owned()))
        })
        .sorted_unstable_by_key(|key| key.0.clone())
        .map(|(name, path_of_contracts)| {
            let mod_name = name.clone().to_case(Case::Snake);
            format!(
                r#"#[rustfmt::skip]
pub mod {mod_name} {{
    alloy::sol!(
        #[allow(missing_docs)]
        #[sol(rpc)]
        #[derive(Debug, PartialEq, Eq,Hash, serde::Serialize, serde::Deserialize)]
        {name},
        "{path_of_contracts}"
    );
}}
"#
            )
        })
        .collect::<Vec<_>>();

    let mut f = std::fs::File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(format!("{this_dir}{BINDINGS_PATH}"))
        .unwrap();

    for contract_build in sol_macro_invocation {
        write!(&mut f, "{contract_build}").expect("failed to write sol macro to contract");
    }
}

pub fn workspace_dir() -> std::path::PathBuf {
    let output = std::process::Command::new(env!("CARGO"))
        .arg("locate-project")
        .arg("--workspace")
        .arg("--message-format=plain")
        .output()
        .unwrap()
        .stdout;
    let cargo_path = std::path::Path::new(std::str::from_utf8(&output).unwrap().trim());
    cargo_path.parent().unwrap().to_path_buf()
}
