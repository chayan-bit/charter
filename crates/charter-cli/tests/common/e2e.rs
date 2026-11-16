//! Helpers for the end-to-end tests: isolated users driving the real binaries
//! against one file-persisted MemArkiv chain.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use charter_store::fake::FileMemArkiv;
use charter_store::{Address, U256};

/// An isolated user: own `$HOME`, `$CHARTER_HOME` and git config, shared fake chain.
pub struct User {
    pub home: PathBuf,
    pub log: PathBuf,
}

pub fn bin_dir() -> PathBuf {
    let helper = PathBuf::from(env!("CARGO_BIN_EXE_git-remote-arkiv"));
    helper.parent().expect("bin dir").to_path_buf()
}

impl User {
    pub fn new(root: &Path, name: &str, log: &Path) -> Self {
        let home = root.join(name);
        std::fs::create_dir_all(&home).expect("home");
        Self {
            home,
            log: log.to_path_buf(),
        }
    }

    pub fn cmd(&self, program: &str, dir: &Path) -> Command {
        let path = std::env::join_paths(std::iter::once(bin_dir()).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )))
        .expect("PATH");
        let mut c = Command::new(program);
        c.current_dir(dir)
            .env("PATH", path)
            .env("HOME", &self.home)
            .env("CHARTER_HOME", self.home.join(".charter"))
            .env("CHARTER_BACKEND", format!("mem:{}", self.log.display()))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("CHARTER_DATA_LIFETIME")
            .env_remove("CHARTER_CONTROL_LIFETIME");
        c
    }

    pub fn run(&self, program: &str, dir: &Path, args: &[&str]) -> Output {
        self.cmd(program, dir).args(args).output().expect("spawn")
    }

    pub fn ok(&self, program: &str, dir: &Path, args: &[&str]) -> String {
        let out = self.run(program, dir, args);
        assert!(
            out.status.success(),
            "{program} {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    pub fn git(&self, dir: &Path, args: &[&str]) -> String {
        let mut full = vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ];
        full.extend_from_slice(args);
        self.ok("git", dir, &full).trim().to_owned()
    }

    pub fn arkiv(&self, args: &[&str]) -> String {
        self.ok(env!("CARGO_BIN_EXE_chart"), &self.home, args)
    }
}

pub fn field<'a>(out: &'a str, key: &str) -> &'a str {
    out.lines()
        .find_map(|l| l.strip_prefix(key))
        .map(str::trim)
        .unwrap_or_else(|| panic!("no {key} in {out}"))
}

pub fn fund(log: &Path, wallet: &str) {
    let mem = FileMemArkiv::open(log, charter_cli::backend::MEM_CHAIN_ID).expect("open");
    let addr: Address = wallet.parse().expect("address");
    mem.fund(addr, U256::from(10u128.pow(24))).expect("fund");
}

pub fn write(dir: &Path, name: &str, bytes: &[u8]) {
    std::fs::write(dir.join(name), bytes).expect("write");
}
