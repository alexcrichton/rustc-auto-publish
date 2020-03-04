#[macro_use]
extern crate serde_derive;
extern crate curl;
extern crate flate2;
extern crate semver;
extern crate serde_json;
extern crate tar;
extern crate tempdir;
extern crate toml;

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::str;
use std::thread;
use std::time::Duration;

const PREFIX: &str = "rustc-ap";

fn main() {
    let token = std::env::args().nth(1);
    let commit = latest_master_commit(&token);
    println!("latest commit: {}", commit);

    let tmpdir = tempdir::TempDir::new("foo").unwrap();
    let tmpdir = tmpdir.path();
    let dst = tmpdir.join(format!("rust-{}", commit));
    let ok = dst.join(".ok");
    if !ok.exists() {
        download_src(&tmpdir, &commit);
    }

    let target_crates = vec![
        RustcApCrate {
            name: "rustc_ast",
            dir: "src/librustc_ast",
        },
        RustcApCrate {
            name: "rustc_parse",
            dir: "src/librustc_parse",
        },
    ];

    println!("learning about the dependency graph");
    let rustc_packages = get_rustc_packages(&target_crates, &dst);
    let mut crates = Vec::new();
    let mut seen = HashSet::new();

    for RustcPackageInfo { package, metadata } in rustc_packages.iter() {
        fill(&metadata, &package, &mut crates, &mut seen);
    }

    let version_to_publish = get_version_to_publish(&crates);
    println!("going to publish {}", version_to_publish);

    for p in crates.iter() {
        publish(p, &commit, &version_to_publish);

        // Give the crates time to make their way into the index
        thread::sleep(Duration::from_secs(10));
    }
}

fn latest_master_commit(token: &Option<String>) -> String {
    println!("Learning rustc's version");
    let mut easy = curl::easy::Easy::new();
    easy.get(true).unwrap();
    easy.url("https://api.github.com/repos/rust-lang/rust/commits/master")
        .unwrap();
    if let Some(token) = token {
        easy.username("x-access-token").unwrap();
        easy.password(token).unwrap();
    }
    let mut headers = curl::easy::List::new();
    headers
        .append("Accept: application/vnd.github.VERSION.sha")
        .unwrap();
    headers.append("User-Agent: foo").unwrap();
    easy.http_headers(headers).unwrap();
    easy.follow_location(true).unwrap();
    let mut data = Vec::new();
    {
        let mut t = easy.transfer();
        t.write_function(|d| {
            data.extend_from_slice(d);
            Ok(d.len())
        })
        .unwrap();
        t.perform().unwrap();
    }
    String::from_utf8(data).unwrap()
}

fn download_src(dst: &Path, commit: &str) {
    println!("downloading source tarball");
    let mut easy = curl::easy::Easy::new();

    let url = format!(
        "https://github.com/rust-lang/rust/archive/{}.tar.gz",
        commit
    );
    easy.get(true).unwrap();
    easy.url(&url).unwrap();
    easy.follow_location(true).unwrap();
    let mut data = Vec::new();
    {
        let mut t = easy.transfer();
        t.write_function(|d| {
            data.extend_from_slice(d);
            Ok(d.len())
        })
        .unwrap();
        t.perform().unwrap();
    }
    assert_eq!(easy.response_code().unwrap(), 200);
    let mut archive = tar::Archive::new(flate2::bufread::GzDecoder::new(&data[..]));
    archive.unpack(dst).unwrap();

    let root = dst.join(format!("rust-{}", commit));
    fs::rename(root.join("Cargo.toml"), root.join("Cargo.toml.bk")).unwrap();

    File::create(&root.join(".ok")).unwrap();
}

fn get_rustc_packages(target_crates: &[RustcApCrate], dst: &Path) -> Vec<RustcPackageInfo> {
    let mut packages = Vec::new();

    for RustcApCrate { name, dir } in target_crates.iter() {
        let metadata = Command::new("cargo")
            .arg("+nightly")
            .current_dir(dst.join(dir))
            .arg("metadata")
            .arg("--format-version=1")
            .output()
            .expect("failed to execute cargo");
        if !metadata.status.success() {
            panic!("failed to run rustc: {:?}", metadata);
        }
        let output = str::from_utf8(&metadata.stdout).unwrap();
        let output: Metadata = serde_json::from_str(output).unwrap();

        let rustc_package = output
            .packages
            .iter()
            .find(|p| p.name == *name)
            .expect(&format!("failed to find {}", &name))
            .clone();

        packages.push(RustcPackageInfo {
            package: rustc_package,
            metadata: output,
        })
    }

    packages
}

fn fill<'a>(
    output: &'a Metadata,
    pkg: &'a Package,
    pkgs: &mut Vec<&'a Package>,
    seen: &mut HashSet<&'a str>,
) {
    if !seen.insert(&pkg.name) {
        return;
    }
    let node = output
        .resolve
        .nodes
        .iter()
        .find(|n| n.id == pkg.id)
        .expect("failed to find resolve node for package");
    for dep in node.dependencies.iter() {
        let pkg = output.packages.iter().find(|p| p.id == *dep).unwrap();
        if pkg.source.is_none() {
            fill(output, pkg, pkgs, seen);
        }
    }
    pkgs.push(pkg);
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    resolve: Resolve,
}

#[derive(Deserialize, Clone)]
struct Package {
    id: String,
    name: String,
    source: Option<String>,
    manifest_path: String,
}

#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<ResolveNode>,
}

#[derive(Deserialize)]
struct ResolveNode {
    id: String,
    dependencies: Vec<String>,
}

struct RustcApCrate<'a> {
    name: &'a str,
    dir: &'a str,
}

struct RustcPackageInfo {
    package: Package,
    metadata: Metadata,
}

fn get_version_to_publish(crates: &[&Package]) -> semver::Version {
    let mut cur = crates.iter().map(|p| get_current_version(p)).max().unwrap();
    cur.major += 1;
    return cur;
}

fn get_current_version(pkg: &Package) -> semver::Version {
    println!("fetching current version of {}", pkg.name);
    let mut easy = curl::easy::Easy::new();

    let url = format!("https://crates.io/api/v1/crates/{}-{}", PREFIX, pkg.name);
    let mut list = curl::easy::List::new();
    list.append("User-Agent: rustc-auto-publish").unwrap();
    easy.get(true).unwrap();
    easy.http_headers(list).unwrap();
    easy.url(&url).unwrap();
    easy.follow_location(true).unwrap();
    let mut data = Vec::new();
    {
        let mut t = easy.transfer();
        t.write_function(|d| {
            data.extend_from_slice(d);
            Ok(d.len())
        })
        .unwrap();
        t.perform().unwrap();
    }
    if easy.response_code().unwrap() == 404 {
        return semver::Version::parse("0.0.0").unwrap();
    }

    assert_eq!(easy.response_code().unwrap(), 200);

    let output: Output = serde_json::from_slice(&data).unwrap();

    return output.krate.max_version;

    #[derive(Deserialize)]
    struct Output {
        #[serde(rename = "crate")]
        krate: Crate,
    }

    #[derive(Deserialize)]
    struct Crate {
        max_version: semver::Version,
    }
}

fn publish(pkg: &Package, commit: &str, vers: &semver::Version) {
    println!("publishing {} {}", pkg.name, vers);

    let mut toml = String::new();
    File::open(&pkg.manifest_path)
        .unwrap()
        .read_to_string(&mut toml)
        .unwrap();
    let mut toml: toml::Value = toml.parse().unwrap();
    {
        let toml = toml.as_table_mut().unwrap();

        if let Some(p) = toml.get_mut("package") {
            let p = p.as_table_mut().unwrap();

            // Update the package's name and version to be consistent with what
            // we're publishing, which is a new version of these two and isn't
            // what is actually written down.
            let name = format!("{}-{}", PREFIX, pkg.name);
            p.insert("name".to_string(), name.into());
            p.insert("version".to_string(), vers.to_string().into());

            // Fill in some other metadata which isn't listed currently and
            // helps the crates published be consistent.
            p.insert("license".to_string(), "MIT / Apache-2.0".to_string().into());
            p.insert(
                "description".to_string(),
                format!(
                    "\
                Automatically published version of the package `{}` \
                in the rust-lang/rust repository from commit {} \

                The publishing script for this crate lives at: \
                https://github.com/alexcrichton/rustc-auto-publish
            ",
                    pkg.name, commit
                )
                .into(),
            );
            p.insert(
                "repository".to_string(),
                "https://github.com/rust-lang/rust".to_string().into(),
            );
        }

        // Remove `crate-type` so it's not compiled as a dylib.
        // Also remove `lib` to force rename the crates.
        if let Some(lib) = toml.get_mut("lib") {
            let lib = lib.as_table_mut().unwrap();
            lib.remove("name");
            lib.remove("crate-type");
        }

        // A few changes to dependencies:
        //
        // * Remove `path` dependencies, changing them to crates.io dependencies
        //   at the `vers` specified above
        // * Update the name of `path` dependencies to what we're publishing,
        //   which is crates with a prefix.
        if let Some(deps) = toml.remove("dependencies") {
            let deps = deps
                .as_table()
                .unwrap()
                .iter()
                .map(|(name, dep)| {
                    let table = match dep.as_table() {
                        Some(s) if s.contains_key("path") => s,
                        _ => return (name.clone(), dep.clone()),
                    };
                    let mut new_table = BTreeMap::new();
                    let mut has_package = false;
                    for (k, v) in table {
                        if k == "package" {
                            let new_name = format!("{}-{}", PREFIX, v.as_str().unwrap());
                            new_table.insert(k.to_string(), new_name.into());
                            has_package = true;
                        } else if k != "path" {
                            new_table.insert(k.to_string(), v.clone());
                        }
                    }
                    new_table.insert("version".to_string(), toml::Value::String(vers.to_string()));
                    if !has_package {
                        new_table.insert(
                            "package".to_string(),
                            toml::Value::String(format!("{}-{}", PREFIX, name)),
                        );
                    }
                    (name.clone(), new_table.into())
                })
                .collect::<Vec<_>>();
            toml.insert(
                "dependencies".to_string(),
                toml::Value::Table(deps.into_iter().collect()),
            );
        }
    }

    let toml = toml.to_string();
    File::create(&pkg.manifest_path)
        .unwrap()
        .write_all(toml.as_bytes())
        .unwrap();

    let path = Path::new(&pkg.manifest_path).parent().unwrap();

    alter_lib_rs(path);

    let result = Command::new("cargo")
        .arg("+nightly")
        .arg("publish")
        .arg("--allow-dirty")
        .arg("--no-verify")
        .current_dir(path)
        .status()
        .expect("failed to spawn cargo");
    assert!(result.success());
}

// TODO: this function shouldn't be necessary, we can change upstream libsyntax
//       to not need these modifications.
fn alter_lib_rs(path: &Path) {
    let lib = path.join("lib.rs");
    if !lib.exists() {
        return;
    }
    let mut contents = String::new();
    File::open(&lib)
        .unwrap()
        .read_to_string(&mut contents)
        .unwrap();

    // Inject #![feature(rustc_private)]. This is a hack, let's fix upstream so
    // we don't have to do this.
    let needle = "\n#![feature(";
    if let Some(i) = contents.find(needle) {
        contents.insert_str(i + needle.len(), "rustc_private, ");
    }

    // Delete __build_diagnostic_array!. This is a hack, let's fix upstream so
    // we don't have to do this.
    if let Some(i) = contents.find("__build_diagnostic_array! {") {
        contents.truncate(i);
        contents.push_str("fn _foo() {}\n");
    }

    File::create(&lib)
        .unwrap()
        .write_all(contents.as_bytes())
        .unwrap()
}
