#![forbid(unsafe_code)]

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::{self, BufRead},
    path::Path,
    rc::Rc,
};

use cargo::{
    core::{
        dependency::Kind as DependencyKind,
        resolver::{Resolve, ResolveOpts},
        InternedString, Package, PackageId, PackageIdSpec, Workspace,
    },
    ops::{resolve_ws_with_opts, Packages},
    util::important_paths::find_root_manifest_for_wd,
};
use cargo_platform::Platform;
use colorify::colorify;
use semver::{Version, VersionReq};
use tera::Tera;

use crate::expr::BoolExpr;
use crate::template::BuildPlan;

mod expr;
mod manifest;
mod platform;
mod template;

type Feature<'a> = &'a str;
type PackageName<'a> = &'a str;
type RootFeature<'a> = (PackageName<'a>, Feature<'a>);

const VERSION_ATTRIBUTE_NAME: &str = "cargo2nixVersion";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let args: Vec<&str> = args.iter().map(AsRef::as_ref).collect();

    match &args[1..] {
        ["--stdout"] | ["-s"] => generate_cargo_nix(io::stdout().lock()),
        ["--file"] | ["-f"] => write_to_file("Cargo.nix"),
        ["--file", file] | ["-f", file] => write_to_file(file),
        ["--help"] | ["-h"] => print_help(),
        ["--version"] | ["-v"] => println!("{}", version()),
        [] => print_help(),
        _ => {
            println!("Invalid arguments: {:?}", &args[1..]);
            println!("\nTry again, with help: \n");
            print_help();
        }
    }
}

fn version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("parse CARGO_PKG_VERSION")
}

fn read_version_attribute(path: &Path) -> Version {
    let file = fs::File::open(path).expect(&format!("Couldn't open file {}", path.display()));
    io::BufReader::new(file)
        .lines()
        .filter_map(|line| line.ok())
        .find(|line| line.trim_start().starts_with(VERSION_ATTRIBUTE_NAME))
        .and_then(|s| {
            if let Some(i) = s.find('"') {
                if let Some(j) = s.rfind('"') {
                    return Some(Version::parse(&s[i + 1..j]).expect("parse version attribute"));
                }
            }
            None
        })
        .expect(&format!(
            "{} not found in {}",
            VERSION_ATTRIBUTE_NAME,
            path.display()
        ))
}

fn version_req(path: &Path) -> (VersionReq, Version) {
    let ver = read_version_attribute(path);
    let requirement = format!(">={}.{}", ver.major, ver.minor);
    (
        VersionReq::parse(&requirement).expect(&format!(
            "parse {} found in {}",
            requirement,
            path.display()
        )),
        ver,
    )
}

fn print_help() {
    println!("cargo2nix-{}\n", version());
    println!("$ cargo2nix                        # Print the help");
    println!("$ cargo2nix -s,--stdout            # Output to stdout");
    println!("$ cargo2nix -f,--file              # Output to Cargo.nix");
    println!("$ cargo2nix -f,--file <file>       # Output to the given file");
    println!("$ cargo2nix -v,--version           # Print version of cargo2nix");
    println!("$ cargo2nix -h,--help              # Print the help");
}

fn write_to_file(file: impl AsRef<Path>) {
    let path = file.as_ref();
    if path.exists() {
        let (vers_req, ver) = version_req(path);
        if !vers_req.matches(&version()) {
            println!(
                colorify!(red_bold: "\nVersion requirement {} [{}]"),
                vers_req, ver
            );
            println!(
                colorify!(red: "\nYour cargo2nix version is {}, whereas the file '{}' was generated by a newer version of cargo2nix."),
                version(),
                path.display()
            );
            println!(
                colorify!(red: "Please upgrade your cargo2nix ({}) to proceed.\n"),
                vers_req
            );
            return;
        }
        println!(
            colorify!(green_bold: "\nVersion {} matches the requirement {} [{}]\n"),
            version(),
            vers_req,
            ver
        );
        print!(
            "warning: do you want to overwrite '{}'? yes/no: ",
            path.display()
        );
        io::Write::flush(&mut io::stdout()).expect("flush stdout buffer");
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .expect("failed to read input");
        if line.trim() != "yes" {
            println!("aborted!");
            return;
        }
    }

    let mut temp_file = tempfile::Builder::new()
        .tempfile()
        .expect("could not create new temporary file");

    generate_cargo_nix(&mut temp_file);

    temp_file
        .persist(path)
        .unwrap_or_else(|e| panic!("could not write file to {}: {}", path.display(), e));
}

fn generate_cargo_nix(mut out: impl io::Write) {
    let config = {
        let mut c = cargo::Config::default().unwrap();
        c.configure(0, None, &None, false, true, false, &None, &[])
            .unwrap();
        c
    };
    let root_manifest_path = find_root_manifest_for_wd(config.cwd()).unwrap();
    let ws = Workspace::new(&root_manifest_path, &config).unwrap();

    let resolve = resolve_ws_with_opts(
        &ws,
        ResolveOpts {
            dev_deps: true,
            features: Rc::new(Default::default()),
            all_features: true,
            uses_default_features: true,
        },
        &Packages::All.to_package_id_specs(&ws).unwrap(),
    )
    .unwrap();

    let pkgs_by_id = resolve
        .pkg_set
        .get_many(resolve.pkg_set.package_ids())
        .unwrap()
        .iter()
        .map(|pkg| (pkg.package_id(), *pkg))
        .collect::<HashMap<_, _>>();

    let mut rpkgs_by_id = resolve
        .pkg_set
        .get_many(resolve.pkg_set.package_ids())
        .unwrap()
        .iter()
        .map(|pkg| {
            (
                pkg.package_id(),
                ResolvedPackage::new(pkg, &pkgs_by_id, &resolve.targeted_resolve),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let root_pkgs = ws.members().collect::<Vec<_>>();
    for pkg in root_pkgs.iter() {
        let pkg_ws = Workspace::new(pkg.manifest_path(), &config).unwrap();
        mark_required(pkg, &pkg_ws, &mut rpkgs_by_id);
        for feature in all_features(&pkg) {
            activate(pkg, feature, &pkg_ws, &mut rpkgs_by_id);
        }
    }

    simplify_optionality(rpkgs_by_id.values_mut(), root_pkgs.len());
    let profiles = manifest::extract_profiles(&fs::read(&root_manifest_path).unwrap());

    let plan = BuildPlan::from_items(root_pkgs, profiles, rpkgs_by_id, config.cwd());
    let mut tera = Tera::default();
    tera.add_raw_template(
        "Cargo.nix.tera",
        include_str!("../templates/Cargo.nix.tera"),
    )
    .expect("error adding template");
    let context = tera::Context::from_serialize(plan).unwrap();
    write!(out, "{}", tera.render("Cargo.nix.tera", &context).unwrap()).expect("write error")
}

fn simplify_optionality<'a, 'b: 'a>(
    rpkgs: impl IntoIterator<Item = &'a mut ResolvedPackage<'b>>,
    n_root_pkgs: usize,
) {
    for rpkg in rpkgs.into_iter() {
        for optionality in rpkg.iter_optionality_mut() {
            if let Optionality::Optional {
                ref required_by_pkgs,
                ..
            } = optionality
            {
                if required_by_pkgs.len() == n_root_pkgs {
                    // This dependency/feature of this package is required by any of the root packages.
                    *optionality = Optionality::Required;
                }
            }
        }

        // Dev dependencies can't be optional.
        rpkg.deps
            .iter_mut()
            .filter(|((_, kind), _)| *kind == DependencyKind::Development)
            .for_each(|(_, d)| d.optionality = Optionality::Required);

        if all_eq(rpkg.iter_optionality_mut()) {
            // This package is always required by a subset of the root packages with the same set of features.
            rpkg.iter_optionality_mut()
                .for_each(|o| *o = Optionality::Required);
        }
    }
}

fn all_features<'a>(p: &'a Package) -> impl 'a + Iterator<Item = Feature<'a>> {
    let features = p.summary().features();
    features
        .keys()
        .map(|k| k.as_str())
        .chain(
            p.dependencies()
                .iter()
                .filter(|d| d.is_optional())
                .map(|d| d.name_in_toml().as_str()),
        )
        .chain(if features.contains_key("default") {
            None
        } else {
            Some("default")
        })
}

fn is_proc_macro(p: &Package) -> bool {
    use cargo::core::{LibKind, TargetKind};

    p.targets()
        .iter()
        .filter_map(|t| match t.kind() {
            TargetKind::Lib(kinds) => Some(kinds.iter()),
            _ => None,
        })
        .flatten()
        .any(|k| *k == LibKind::ProcMacro)
}

/// Traverses the whole dependency graph starting at `pkg` and marks required packages and features.
fn mark_required(
    root_pkg: &Package,
    ws: &Workspace,
    rpkgs_by_id: &mut BTreeMap<PackageId, ResolvedPackage>,
) {
    let resolve = resolve_ws_with_opts(
        ws,
        ResolveOpts {
            dev_deps: true,
            features: Rc::new(Default::default()),
            all_features: false,
            uses_default_features: false,
        },
        &[PackageIdSpec::from_package_id(root_pkg.package_id())],
    )
    .unwrap();

    let root_pkg_name = root_pkg.name().as_str();
    // Dependencies that are activated, even when no features are activated, must be required.
    for id in resolve.targeted_resolve.iter() {
        let rpkg = rpkgs_by_id.get_mut(&id).unwrap();
        for feature in resolve.targeted_resolve.features(id).iter() {
            rpkg.features
                .get_mut(feature.as_str())
                .unwrap()
                .required_by(root_pkg_name);
        }

        for (dep_id, _) in resolve.targeted_resolve.deps(id) {
            for dep in rpkg.iter_deps_with_id_mut(dep_id) {
                dep.optionality.required_by(root_pkg_name);
            }
        }
    }
}

fn activate<'a>(
    pkg: &'a Package,
    feature: Feature<'a>,
    ws: &Workspace,
    rpkgs_by_id: &mut BTreeMap<PackageId, ResolvedPackage<'a>>,
) {
    let resolve = resolve_ws_with_opts(
        ws,
        ResolveOpts {
            dev_deps: true,
            features: Rc::new({
                let mut s = BTreeSet::new();
                if feature != "default" {
                    s.insert(InternedString::new(feature));
                }
                s
            }),
            all_features: false,
            uses_default_features: feature == "default",
        },
        &[PackageIdSpec::from_package_id(pkg.package_id())],
    )
    .unwrap();

    let root_feature = (pkg.name().as_str(), feature);
    for id in resolve.targeted_resolve.iter() {
        let rpkg = rpkgs_by_id.get_mut(&id).unwrap();
        for feature in resolve.targeted_resolve.features(id).iter() {
            rpkg.features
                .get_mut(feature.as_str())
                .unwrap()
                .activated_by(root_feature);
        }

        for (dep_id, _) in resolve.targeted_resolve.deps(id) {
            for dep in rpkg.iter_deps_with_id_mut(dep_id) {
                dep.optionality.activated_by(root_feature)
            }
        }
    }
}

#[derive(Debug)]
pub struct ResolvedPackage<'a> {
    pkg: &'a Package,
    deps: BTreeMap<(PackageId, DependencyKind), ResolvedDependency<'a>>,
    features: BTreeMap<Feature<'a>, Optionality<'a>>,
    checksum: Option<Cow<'a, str>>,
}

#[derive(Debug)]
struct ResolvedDependency<'a> {
    extern_name: String,
    pkg: &'a Package,
    optionality: Optionality<'a>,
    platforms: Option<Vec<&'a Platform>>,
}

#[derive(PartialEq, Eq, Debug)]
enum Optionality<'a> {
    Required,
    Optional {
        required_by_pkgs: BTreeSet<PackageName<'a>>,
        activated_by_features: BTreeSet<RootFeature<'a>>,
    },
}

impl<'a> ResolvedPackage<'a> {
    fn new(
        pkg: &'a Package,
        pkgs_by_id: &HashMap<PackageId, &'a Package>,
        resolve: &'a Resolve,
    ) -> Self {
        let mut deps = BTreeMap::new();
        resolve
            .deps(pkg.package_id())
            .filter_map(|(dep_id, deps)| {
                let dep_pkg = pkgs_by_id[&dep_id];
                let extern_name = resolve
                    .extern_crate_name(
                        pkg.package_id(),
                        dep_id,
                        dep_pkg.targets().iter().find(|t| t.is_lib())?,
                    )
                    .ok()?;

                Some(
                    deps.iter()
                        .map(move |dep| (dep_id, dep, dep_pkg, extern_name.clone())),
                )
            })
            .flatten()
            .for_each(|(dep_id, dep, dep_pkg, extern_name)| {
                let rdep = deps
                    .entry((dep_id, dep.kind()))
                    .or_insert(ResolvedDependency {
                        extern_name,
                        pkg: dep_pkg,
                        optionality: Optionality::default(),
                        platforms: Some(Vec::new()),
                    });

                match (dep.platform(), rdep.platforms.as_mut()) {
                    (Some(platform), Some(platforms)) => platforms.push(platform),
                    (None, _) => rdep.platforms = None,
                    _ => {}
                }
            });

        Self {
            pkg,
            deps,
            features: resolve
                .features(pkg.package_id())
                .iter()
                .map(|feature| (feature.as_str(), Optionality::default()))
                .collect(),
            checksum: resolve
                .checksums()
                .get(&pkg.package_id())
                .and_then(|s| s.as_ref().map(|s| Cow::Borrowed(s.as_str())))
                .or_else(|| {
                    let source_id = pkg.package_id().source_id();
                    if source_id.is_git() {
                        Some(Cow::Owned(
                            prefetch_git(
                                source_id.url().as_str(),
                                source_id.precise().unwrap_or_else(|| {
                                    panic!("no precise git reference for {}", pkg.package_id())
                                }),
                            )
                            .unwrap_or_else(|e| {
                                panic!(
                                    "failed to compute SHA256 for {} using nix-prefetch-git: {}",
                                    pkg.package_id(),
                                    e
                                )
                            }),
                        ))
                    } else {
                        None
                    }
                }),
        }
    }

    fn iter_deps_with_id_mut(
        &mut self,
        id: PackageId,
    ) -> impl Iterator<Item = &mut ResolvedDependency<'a>> {
        self.deps
            .range_mut((id, DependencyKind::Normal)..=(id, DependencyKind::Build))
            .map(|(_, dep)| dep)
    }

    fn iter_optionality_mut(&mut self) -> impl Iterator<Item = &mut Optionality<'a>> {
        self.deps
            .iter_mut()
            .filter(|((_, kind), _)| *kind != DependencyKind::Development)
            .map(|(_, d)| &mut d.optionality)
            .chain(self.features.values_mut())
    }
}

impl<'a> Default for Optionality<'a> {
    fn default() -> Self {
        Optionality::Optional {
            required_by_pkgs: Default::default(),
            activated_by_features: Default::default(),
        }
    }
}

impl<'a> Optionality<'a> {
    fn activated_by(&mut self, (pkg_name, feature): RootFeature<'a>) {
        if let Optionality::Optional {
            required_by_pkgs,
            activated_by_features,
        } = self
        {
            if !required_by_pkgs.contains(pkg_name) {
                activated_by_features.insert((pkg_name, feature));
            }
        }
    }

    fn required_by(&mut self, pkg_name: PackageName<'a>) {
        if let Optionality::Optional {
            required_by_pkgs, ..
        } = self
        {
            required_by_pkgs.insert(pkg_name);
        }
    }

    fn to_expr(&self, root_features_var: &str) -> BoolExpr {
        use self::BoolExpr::*;

        match self {
            Optionality::Required => True,
            Optionality::Optional {
                activated_by_features,
                required_by_pkgs,
            } => {
                BoolExpr::ors(
                    activated_by_features
                        .iter()
                        .map(|root_feature| {
                            Single(format!(
                                "{} ? {:?}",
                                root_features_var,
                                display_root_feature(*root_feature)
                            ))
                        })
                        .chain(required_by_pkgs.iter().map(|pkg_name| {
                            Single(format!("{} ? {:?}", root_features_var, pkg_name))
                        })),
                )
            }
        }
    }
}

fn display_root_feature((pkg_name, feature): RootFeature) -> String {
    format!("{}/{}", pkg_name, feature)
}

fn prefetch_git(url: &str, rev: &str) -> Result<String, Box<dyn std::error::Error>> {
    use std::process::{Command, Output};

    let Output {
        stdout,
        stderr,
        status,
    } = Command::new("nix-prefetch-git")
        .arg("--quiet")
        .args(&["--url", url])
        .args(&["--rev", rev])
        .output()?;

    if status.success() {
        Ok(serde_json::from_slice::<serde_json::Value>(&stdout)?
            .get("sha256")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or("unexpected JSON output")?)
    } else {
        Err(format!(
            "process failed with stderr {:?}",
            String::from_utf8(stderr)
        ))?
    }
}

fn all_eq<T, I>(i: I) -> bool
where
    I: IntoIterator<Item = T>,
    T: PartialEq,
{
    let mut iter = i.into_iter();
    let first = match iter.next() {
        Some(x) => x,
        None => return true,
    };

    return iter.all(|x| x == first);
}
