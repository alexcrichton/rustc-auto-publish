extern crate cargo;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate tempdir;
extern crate curl;
extern crate tar;
extern crate flate2;
extern crate semver;
extern crate toml;
extern crate crates_io;

use std::collections::{HashSet, BTreeMap};
use std::rc::Rc;
use std::fs::{self, File};
use std::path::{PathBuf, Path};
use std::process::Command;
use std::str;

const PREFIX: &str = "rustc-ap";

fn main() {
    println!("Learning rustc's version");
    let output = Command::new("rustc")
        .arg("+nightly")
        .arg("-vV")
        .arg("sysroot")
        .output()
        .expect("failed to spawn rustc");
    if !output.status.success() {
        panic!("failed to run rustc: {:?}", output);
    }

    let output = str::from_utf8(&output.stdout).unwrap();
    let commit = output.lines()
        .find(|l| l.starts_with("commit-hash"))
        .expect("failed to find commit hash")
        .split(' ')
        .nth(1)
        .unwrap();

    let tmpdir = PathBuf::from("tmp");
    let dst = tmpdir.join(format!("rust-{}", commit));
    let ok = dst.join(".ok");
    if !ok.exists() {
        download_src(&tmpdir, commit);
    }

    println!("learning about the dependency graph");
    let metadata = Command::new("cargo")
        .arg("+nightly")
        .current_dir(dst.join("src/libsyntax"))
        .arg("metadata")
        .arg("--format-version=1")
        .output()
        .expect("failed to execute cargo");
    if !metadata.status.success() {
        panic!("failed to run rustc: {:?}", metadata);
    }
    let output = str::from_utf8(&metadata.stdout).unwrap();
    let output: Metadata = serde_json::from_str(output).unwrap();

    let syntax = output.packages
        .iter()
        .find(|p| p.name == "syntax")
        .expect("failed to find libsyntax");

    let mut crates = Vec::new();
    fill(&output, &syntax, &mut crates, &mut HashSet::new());

    let version_to_publish = get_version_to_publish();
    println!("going to publish {}", version_to_publish);

    for p in crates.iter() {
        publish(p, &commit, &version_to_publish);
    }
}

fn download_src(dst: &Path, commit: &str) {
    println!("downloading source tarball");
    let mut easy = curl::easy::Easy::new();

    let url = format!("https://github.com/rust-lang/rust/archive/{}.tar.gz",
                      commit);
    easy.get(true).unwrap();
    easy.url(&url).unwrap();
    easy.follow_location(true).unwrap();
    let mut data = Vec::new();
    {
        let mut t = easy.transfer();
        t.write_function(|d| {
            data.extend_from_slice(d);
            Ok(d.len())
        }).unwrap();
        t.perform().unwrap();
    }
    assert_eq!(easy.response_code().unwrap(), 200);
    let mut archive = tar::Archive::new(flate2::bufread::GzDecoder::new(&data[..]));
    archive.unpack(dst).unwrap();

    let root = dst.join(format!("rust-{}", commit));
    fs::rename(root.join("src/Cargo.toml"), root.join("src/Cargo.toml.bk")).unwrap();

    File::create(&root.join(".ok")).unwrap();
}

fn fill<'a>(output: &'a Metadata,
            pkg: &'a Package,
            pkgs: &mut Vec<&'a Package>,
            seen: &mut HashSet<&'a str>) {
    if !seen.insert(&pkg.name) {
        return
    }
    let node = output.resolve.nodes
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

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    version: String,
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

fn get_version_to_publish() -> semver::Version {
    let mut cur = get_current_version();
    cur.major += 1;
    return cur
}

fn get_current_version() -> semver::Version {
    println!("fetching current version");
    let mut easy = curl::easy::Easy::new();

    let url = format!("https://crates.io/api/v1/crates/{}-syntax", PREFIX);
    easy.get(true).unwrap();
    easy.url(&url).unwrap();
    easy.follow_location(true).unwrap();
    let mut data = Vec::new();
    {
        let mut t = easy.transfer();
        t.write_function(|d| {
            data.extend_from_slice(d);
            Ok(d.len())
        }).unwrap();
        t.perform().unwrap();
    }
    if easy.response_code().unwrap() == 404 {
        return semver::Version::parse("0.0.0").unwrap()
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
    let mut config = cargo::util::Config::default().unwrap();
    config.configure(0, None, &None, false, true, &[]).unwrap();
    let ws_temp = cargo::core::Workspace::new(pkg.manifest_path.as_ref(), &config).unwrap();
    let pkg = remap_pkg(ws_temp.current().unwrap(), commit, vers, &config);
    let ws = cargo::core::Workspace::ephemeral(pkg, &config, None, false).unwrap();

    let pkg = ws.current().unwrap();
    alter_lib_rs(pkg.manifest_path().parent().unwrap());

    let tarball = cargo::ops::package(&ws, &cargo::ops::PackageOpts {
        config: &config,
        verify: false,
        list: false,
        check_metadata: false,
        allow_dirty: true,
        target: None,
        jobs: None,
        registry: None,
    }).unwrap().unwrap();

    let deps = pkg.dependencies().iter().map(|dep| {
        crates_io::NewCrateDependency {
            optional: dep.is_optional(),
            default_features: dep.uses_default_features(),
            name: dep.name().to_string(),
            features: dep.features().to_vec(),
            version_req: dep.version_req().to_string(),
            target: dep.platform().map(|s| s.to_string()),
            kind: match dep.kind() {
                cargo::core::dependency::Kind::Normal => "normal",
                cargo::core::dependency::Kind::Build => "build",
                cargo::core::dependency::Kind::Development => "dev",
            }.to_string(),
            registry: None,
        }
    }).collect::<Vec<_>>();
    let manifest = pkg.manifest();
    let cargo::core::manifest::ManifestMetadata {
        ref authors, ref description, ref homepage, ref documentation,
        ref keywords, ref readme, ref repository, ref license, ref license_file,
        ref categories, ref badges,
    } = *manifest.metadata();

    let api_host = "https://crates.io/".to_string();
    let token = config.get_string("registry.token").unwrap().unwrap().val;

    let mut registry = crates_io::Registry::new(api_host, Some(token));

    registry.publish(&crates_io::NewCrate {
        name: pkg.name().to_string(),
        vers: pkg.version().to_string(),
        deps: deps,
        features: pkg.summary().features().clone(),
        authors: authors.clone(),
        description: description.clone(),
        homepage: homepage.clone(),
        documentation: documentation.clone(),
        keywords: keywords.clone(),
        categories: categories.clone(),
        readme: None,
        readme_file: readme.clone(),
        repository: repository.clone(),
        license: license.clone(),
        license_file: license_file.clone(),
        badges: badges.clone(),
    }, tarball.file()).unwrap();
}

fn remap_pkg(pkg: &cargo::core::Package,
             commit: &str,
             vers: &semver::Version,
             config: &cargo::util::Config)
    -> cargo::core::Package
{
    let manifest = pkg.manifest();
    let summary = manifest.summary();
    let crates_io = cargo::core::SourceId::crates_io(config).unwrap();

    let mut dependencies = summary.dependencies()
        .iter()
        .map(|d| {
            if !d.source_id().is_path() {
                return d.clone()
            }

            // Translate all path dependencies to depend on our version of the
            // crates which have a new name (PREFIX) attached to them.
            let mut dep = cargo::core::Dependency::parse_no_deprecated(
                &format!("{}-{}", PREFIX, d.name()),
                Some(&vers.to_string()[..]),
                &crates_io,
            ).unwrap();
            dep.set_kind(d.kind());
            dep.set_features(d.features().to_vec());
            dep.set_default_features(d.uses_default_features());
            dep.set_optional(d.is_optional());
            dep.set_platform(d.platform().cloned());
            return dep
        })
        .collect::<Vec<_>>();

    // Inject a dependency on `term` that the crates actually pick up from the
    // sysroot (it's a dependency of libtest). Most crates don't actually depend
    // on `term` but some do and it's just easy to add a dependency to
    // everything for now.
    dependencies.push(cargo::core::Dependency::parse_no_deprecated(
        "term",
        Some("0.4"),
        &crates_io,
    ).unwrap());

    // Give the summary a new package ID with the new package name
    let summary = cargo::core::Summary::new(
        cargo::core::PackageId::new(
            &format!("{}-{}", PREFIX, pkg.package_id().name()),
            &vers.to_string()[..],
            pkg.package_id().source_id(),
        ).unwrap(),
        dependencies,
        summary.features().clone(),
    ).unwrap();

    let manifest = cargo::core::Manifest::new(
        summary,
        manifest.targets().to_vec(),
        manifest.include().to_vec(),
        manifest.exclude().to_vec(),
        manifest.links().map(|s| s.to_string()),

        // Fill in some hopefully useful metadata for when anyone comes across
        // this.
        cargo::core::manifest::ManifestMetadata {
            authors: vec![
                "The Rust Project Developers".to_string(),
            ],
            keywords: Vec::new(),
            categories: Vec::new(),
            license: Some("MIT / Apache-2.0".to_string()),
            license_file: None,
            description: Some(format!("\
                Automatically published version of the package `{}` \
                in the rust-lang/rust repository from commit {} \
            ", pkg.package_id().name(), commit)),
            readme: None,
            homepage: None,
            repository: Some("https://github.com/rust-lang/rust".to_string()),
            documentation: None,
            badges: Default::default(),
        },
        manifest.profiles().clone(),
        None,
        Vec::new(),
        Default::default(),
        cargo::core::WorkspaceConfig::Member { root: None },
        manifest.features().clone(),
        None,
        Rc::new(map_toml_manifest(pkg, vers, manifest.original())),
    );

    cargo::core::Package::new(manifest, pkg.manifest_path())
}

fn map_toml_manifest(pkg: &cargo::core::Package,
                     version: &semver::Version,
                     manifest: &cargo::util::toml::TomlManifest)
    -> cargo::util::toml::TomlManifest
{
    let mut toml = toml::Value::try_from(manifest).unwrap();
    {
        let toml = toml.as_table_mut().unwrap();

        if let Some(p) = toml.get_mut("package") {
            let p = p.as_table_mut().unwrap();
            let name = format!("{}-{}", PREFIX, pkg.package_id().name());
            p.insert("name".to_string(), toml::Value::String(name));
            p.insert("version".to_string(), toml::Value::String(version.to_string()));
        }

        if let Some(lib) = toml.get_mut("lib") {
            let lib = lib.as_table_mut().unwrap();
            let name = pkg.package_id().name().to_string();
            lib.insert("name".to_string(), toml::Value::String(name));
            lib.remove("crate-type");
        }

        if let Some(deps) = toml.remove("dependencies") {
            toml.insert(
                "dependencies".to_string(),
                toml::Value::Table(deps.as_table().unwrap().iter().map(|(name, dep)| {
                    let table = match dep.as_table() {
                        Some(s) if s.contains_key("path") => s,
                        _ => return (name.clone(), dep.clone()),
                    };
                    let mut new_table = BTreeMap::new();
                    for (k, v) in table {
                        if k != "path" {
                            new_table.insert(k.to_string(), v.clone());
                        }
                    }
                    new_table.insert(
                        "version".to_string(),
                        toml::Value::String(version.to_string()),
                    );
                    (format!("{}-{}", PREFIX, name), new_table.into())
                }).collect()),
            );
        }
    }
    toml.try_into().unwrap()
}

fn alter_lib_rs(path: &Path) {
    let lib = path.join("lib.rs");
    if !lib.exists() {
        return
    }
    let mut contents = cargo::util::paths::read(&lib).unwrap();

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

    cargo::util::paths::write(&lib, contents.as_bytes()).unwrap();
}
