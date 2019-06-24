extern crate cargo_update;
extern crate tabwriter;
extern crate lazysort;
extern crate regex;
extern crate git2;

use std::process::{Command, exit};
use std::io::{Write, stdout};
use tabwriter::TabWriter;
use lazysort::SortedBy;
use std::fmt::Display;
use git2::Repository;
#[cfg(target_os="windows")]
use std::fs::File;
use regex::Regex;
use std::env;
use std::fs;


fn main() {
    let result = actual_main().err().unwrap_or(0);
    exit(result);
}

fn actual_main() -> Result<(), i32> {
    let opts = cargo_update::Options::parse();

    if cfg!(target_os = "windows") {
        let old_version_r = Regex::new(r"cargo-install-update\.exe-v.+").unwrap();
        for old_version in fs::read_dir(env::current_exe().unwrap().parent().unwrap().canonicalize().unwrap())
            .unwrap()
            .map(Result::unwrap)
            .filter(|f| old_version_r.is_match(&f.file_name().into_string().unwrap())) {
            fs::remove_file(old_version.path()).unwrap();
        }
    }

    let crates_file = cargo_update::ops::resolve_crates_file(opts.crates_file.1.clone());
    let http_proxy = cargo_update::ops::find_proxy(&crates_file);
    let configuration = try!(cargo_update::ops::PackageConfig::read(&crates_file.with_file_name(".install_config.toml")));
    let mut packages = cargo_update::ops::installed_main_repo_packages(&crates_file);
    let installed_git_packages = if opts.update_git || (opts.update && opts.install) {
        cargo_update::ops::installed_git_repo_packages(&crates_file)
    } else {
        vec![]
    };

    if !installed_git_packages.is_empty() {
        // Don't install registry packages on top of git ones (#103)
        packages.retain(|p| installed_git_packages.iter().find(|gp| p.name == gp.name).is_none());
    }
    if !opts.filter.is_empty() {
        packages.retain(|p| configuration.get(&p.name).map(|p_cfg| opts.filter.iter().all(|f| f.matches(p_cfg))).unwrap_or(false));
    }
    match (opts.all, opts.to_update.is_empty()) {
        (true, true) => {}
        (true, false) => {
            for pkg in cargo_update::ops::intersect_packages(&packages, &opts.to_update, opts.install).into_iter() {
                if packages.iter().find(|p| p.name == pkg.name).is_none() {
                    packages.push(pkg);
                }
            }
        }
        (false, true) => {
            panic!("No packages to update and -a not specified, this should've been caught by option parser \
                    (please report to http://github.com/nabijaczleweli/cargo-update)")
        }
        (false, false) => packages = cargo_update::ops::intersect_packages(&packages, &opts.to_update, opts.install),
    }

    let registry = try!(cargo_update::ops::get_index_path(&opts.cargo_dir.1).map_err(|e| {
        println!("Couldn't get package repository: {}.", e);
        2
    }));
    let mut registry_repo = try!(Repository::open(&registry).map_err(|_| {
        println!("Failed to open registry repository at {}.", registry.display());
        2
    }));
    try!(cargo_update::ops::update_index(&mut registry_repo,
                                         "https://github.com/rust-lang/crates.io-index",
                                         http_proxy.as_ref().map(String::as_str),
                                         &mut stdout())
        .map_err(|e| {
            println!("Failed to update index repository: {}.", e);
            2
        }));
    let latest_registry = try!(registry_repo.revparse_single("origin/master").map_err(|_| {
        println!("Failed to read master branch of registry repository at {}.", registry.display());
        2
    }));

    for package in &mut packages {
        package.pull_version(&latest_registry.as_commit().unwrap().tree().unwrap(), &registry_repo);
    }

    {
        let mut out = TabWriter::new(stdout());
        writeln!(out, "Package\tInstalled\tLatest\tNeeds update").unwrap();
        for (package, package_target_version) in
            packages.iter()
                .map(|p| (p, configuration.get(&p.name).and_then(|c| c.target_version.as_ref())))
                .sorted_by(|&(ref lhs, lhstv), &(ref rhs, rhstv)| (!lhs.needs_update(lhstv), &lhs.name).cmp(&(!rhs.needs_update(rhstv), &rhs.name))) {
            write!(out, "{}\t", package.name).unwrap();
            if let Some(ref v) = package.version {
                write!(out, "v{}", v).unwrap();
            }
            if let Some(tv) = package_target_version {
                write!(out, "\t{}", tv).unwrap();
            } else if let Some(upd_v) = package.update_to_version() {
                write!(out, "\tv{}", upd_v).unwrap();
            } else {
                write!(out, "\tN/A").unwrap();
            }
            writeln!(out,
                     "\t{}",
                     if package.needs_update(package_target_version) {
                         "Yes"
                     } else {
                         "No"
                     })
                .unwrap();
        }
        writeln!(out).unwrap();
        out.flush().unwrap();
    }

    let mut success_n_global = 0usize;
    let mut errored_global = vec![];
    let mut result_global = None;

    if opts.update {
        if !opts.force {
            packages.retain(|p| p.needs_update(configuration.get(&p.name).and_then(|c| c.target_version.as_ref())));
        }

        packages.retain(|pkg| pkg.update_to_version().is_some());

        if !packages.is_empty() {
            let (success_n, errored, result): (usize, Vec<String>, Option<i32>) = packages.into_iter()
                .map(|package| -> Result<(), (i32, String)> {
                    println!("{} {}",
                             if package.version.is_some() {
                                 "Updating"
                             } else {
                                 "Installing"
                             },
                             package.name);

                    if cfg!(target_os = "windows") && package.version.is_some() && package.name == "cargo-update" {
                        save_cargo_update_exec(package.version.as_ref().unwrap());
                    }

                    let install_res = if let Some(cfg) = configuration.get(&package.name) {
                            Command::new("cargo")
                                .args(&cfg.cargo_args()[..])
                                .arg(&package.name)
                                .arg("--vers")
                                .arg(if let Some(tv) = cfg.target_version.as_ref() {
                                    tv.to_string()
                                } else {
                                    package.update_to_version().unwrap().to_string()
                                })
                                .status()
                        } else {
                            Command::new("cargo")
                                .arg("install")
                                .arg("-f")
                                .arg(&package.name)
                                .arg("--vers")
                                .arg(package.update_to_version().unwrap().to_string())
                                .status()
                        }
                        .unwrap();

                    println!();
                    if !install_res.success() {
                        if cfg!(target_os = "windows") && package.version.is_some() && package.name == "cargo-update" {
                            restore_cargo_update_exec(package.version.as_ref().unwrap());
                        }

                        Err((install_res.code().unwrap_or(-1), package.name))
                    } else {
                        Ok(())
                    }
                })
                .fold((0, vec![], None), |(s, mut e, r), p| match p {
                    Ok(()) => (s + 1, e, r),
                    Err((pr, pn)) => {
                        e.push(pn);
                        (s, e, r.or_else(|| Some(pr)))
                    }
                });

            success_n_global += success_n;

            println!();
            println!("Updated {} package{}.", success_n, if success_n == 1 { "" } else { "s" });
            if !errored.is_empty() && result.is_some() {
                print!("Failed to update ");
                for (i, e) in errored.iter().enumerate() {
                    if i != 0 {
                        print!(", ");
                    }
                    print!("{}", e);
                }
                println!(".");
                println!();

                if opts.update_git {
                    errored_global = errored;
                    result_global = result;
                } else {
                    return Err(result.unwrap());
                }
            }
        } else {
            println!("No packages need updating.");
        }
    }

    if opts.update_git {
        let mut packages = installed_git_packages;

        if !opts.filter.is_empty() {
            packages.retain(|p| configuration.get(&p.name).map(|p_cfg| opts.filter.iter().all(|f| f.matches(p_cfg))).unwrap_or(false));
        }
        if !opts.all {
            packages.retain(|p| opts.to_update.iter().any(|u| p.name == u.0));
        }

        for package in &mut packages {
            package.pull_version(&opts.temp_dir.1, http_proxy.as_ref().map(String::as_str));
        }

        {
            let mut out = TabWriter::new(stdout());
            writeln!(out, "Package\tInstalled\tLatest\tNeeds update").unwrap();
            for package in packages.iter()
                .sorted_by(|lhs, rhs| (!lhs.needs_update(), &lhs.name).cmp(&(!rhs.needs_update(), &rhs.name))) {
                writeln!(out,
                         "{}\t{}\t{}\t{}",
                         package.name,
                         package.id,
                         package.newest_id.as_ref().unwrap(),
                         if package.needs_update() { "Yes" } else { "No" })
                    .unwrap();
            }
            writeln!(out).unwrap();
            out.flush().unwrap();
        }

        if opts.update {
            if !opts.force {
                packages.retain(cargo_update::ops::GitRepoPackage::needs_update);
            }

            if !packages.is_empty() {
                let (success_n, errored, result): (usize, Vec<String>, Option<i32>) = packages.into_iter()
                    .map(|package| -> Result<(), (i32, String)> {
                        println!("Updating {} from {}", package.name, package.url);

                        if cfg!(target_os = "windows") && package.name == "cargo-update" {
                            save_cargo_update_exec(&package.id.to_string());
                        }

                        let install_res = if let Some(cfg) = configuration.get(&package.name) {
                                let mut cmd = Command::new("cargo");
                                cmd.args(&cfg.cargo_args()[..])
                                    .arg("--git")
                                    .arg(&package.url)
                                    .arg(&package.name);
                                if let Some(ref b) = package.branch.as_ref() {
                                    cmd.arg("--branch").arg(b);
                                }
                                cmd.status()
                            } else {
                                let mut cmd = Command::new("cargo");
                                cmd.arg("install")
                                    .arg("-f")
                                    .arg("--git")
                                    .arg(&package.url)
                                    .arg(&package.name);
                                if let Some(ref b) = package.branch.as_ref() {
                                    cmd.arg("--branch").arg(b);
                                }
                                cmd.status()
                            }
                            .unwrap();

                        println!();
                        if !install_res.success() {
                            if cfg!(target_os = "windows") && package.name == "cargo-update" {
                                restore_cargo_update_exec(&package.id.to_string());
                            }

                            Err((install_res.code().unwrap_or(-1), package.name))
                        } else {
                            Ok(())
                        }
                    })
                    .fold((0, vec![], None), |(s, mut e, r), p| match p {
                        Ok(()) => (s + 1, e, r),
                        Err((pr, pn)) => {
                            e.push(pn);
                            (s, e, r.or_else(|| Some(pr)))
                        }
                    });

                success_n_global += success_n;

                println!();
                println!("Updated {} git package{}.", success_n, if success_n == 1 { "" } else { "s" });
                if !errored.is_empty() && result.is_some() {
                    print!("Failed to update ");
                    for (i, e) in errored.iter().enumerate() {
                        if i != 0 {
                            print!(", ");
                        }
                        print!("{}", e);
                    }
                    println!(".");
                    println!();

                    errored_global.extend(errored);

                    if result_global.is_none() {
                        return Err(result.unwrap());
                    }
                }
            } else {
                println!("No git packages need updating.");
            }
        }
    }

    if opts.update {
        println!("Overall updated {} package{}.", success_n_global, if success_n_global == 1 { "" } else { "s" });

        if !errored_global.is_empty() && result_global.is_some() {
            print!("Overall failed to update ");
            for (i, e) in errored_global.iter().enumerate() {
                if i != 0 {
                    print!(", ");
                }
                print!("{}", e);
            }
            println!(".");

            return Err(result_global.unwrap());
        }
    }

    Ok(())
}


/// This way the past-current exec will be "replaced" and we'll get no dupes in .cargo.toml
#[cfg(target_os="windows")]
fn save_cargo_update_exec<D: Display>(version: &D) {
    save_cargo_update_exec_impl(format!("exe-v{}", version));
}

#[cfg(target_os="windows")]
fn save_cargo_update_exec_impl(extension: String) {
    let cur_exe = env::current_exe().unwrap();
    fs::rename(&cur_exe, cur_exe.with_extension(extension)).unwrap();
    File::create(cur_exe).unwrap();
}

#[cfg(target_os="windows")]
fn restore_cargo_update_exec<D: Display>(version: &D) {
    restore_cargo_update_exec_impl(format!("exe-v{}", version))
}

#[cfg(target_os="windows")]
fn restore_cargo_update_exec_impl(extension: String) {
    let cur_exe = env::current_exe().unwrap();
    fs::remove_file(&cur_exe).unwrap();
    fs::rename(cur_exe.with_extension(extension), cur_exe).unwrap();
}


#[cfg(not(target_os="windows"))]
fn save_cargo_update_exec<D: Display>(_: &D) {}

#[cfg(not(target_os="windows"))]
fn restore_cargo_update_exec<D: Display>(_: &D) {}
