use crate::config::Config;
use crate::dep::{DepKind, RootCrate};
use crate::error::{CliError, CliResult};
use crate::graph::DepGraph;
use crate::util;
use std::collections::HashMap;
use std::path::PathBuf;
use toml::Value;

// Map of dep names to their kinds.
pub type DepKindsMap = HashMap<String, Vec<DepKind>>;
// Map of root names to dep kinds maps.
pub type RootDepsMap = HashMap<String, DepKindsMap>;

#[derive(Debug)]
pub struct Project {
    cfg: Config,
}

impl Project {
    pub fn with_config(cfg: Config) -> CliResult<Self> {
        Ok(Project { cfg })
    }

    pub fn graph(self, manifest_path: PathBuf, lock_path: PathBuf) -> CliResult<DepGraph> {
        let (root_crates, root_deps_map) = self.parse_root_deps(&manifest_path)?;

        let mut dg = self.parse_lock_file(lock_path, &dbg!(root_crates), dbg!(root_deps_map))?;

        // Sort the graph.
        dg.topological_sort()?;

        dbg!(&dg);

        // Set the kind of dependency on each dep.
        dg.set_resolved_kind()?;

        if !self.cfg.include_vers {
            dg.show_version_on_duplicates();
        }

        Ok(dbg!(dg))
    }

    /// Builds a list of the dependencies declared in the manifest file.
    pub fn parse_root_deps(
        &self,
        manifest_path: &PathBuf,
    ) -> CliResult<(Vec<RootCrate>, RootDepsMap)> {
        let manifest_toml = util::toml_from_file(manifest_path)?;

        // Get the name and version of the root project.
        let root_crates_tomls = {
            if let Some(table) = manifest_toml.get("package") {
                if let Some(table) = table.as_table() {
                    if let (Some(&Value::String(ref name)), Some(&Value::String(ref ver))) =
                        (table.get("name"), table.get("version"))
                    {
                        let (name, ver) = (name.to_string(), ver.to_string());
                        vec![(RootCrate { name, ver }, manifest_toml)]
                    } else {
                        return Err(CliError::Toml(
                            "No 'name' or 'version' fields in [package] table".into(),
                        ));
                    }
                } else {
                    return Err(CliError::Toml(
                        "Could not parse [package] as a table".into(),
                    ));
                }
            } else {
                // TODO: Check for workspace here.
                return Err(CliError::Toml("No [package] table found".into()));
            }
        };

        let mut root_deps_map = HashMap::new();

        for (root_crate, manifest_toml) in root_crates_tomls.iter() {
            let root_name = &root_crate.name;
            let mut dep_kinds_map = HashMap::new();

            if let Some(table) = manifest_toml.get("dependencies") {
                if let Some(table) = table.as_table() {
                    for (dep_name, dep_table) in table.iter() {
                        if let Some(&Value::Boolean(true)) = dep_table.get("optional") {
                            if self.cfg.optional_deps {
                                add_kind(
                                    &mut dep_kinds_map,
                                    dep_name.to_string(),
                                    DepKind::Optional,
                                );
                            }
                        } else if self.cfg.regular_deps {
                            add_kind(&mut dep_kinds_map, dep_name.to_string(), DepKind::Regular);
                        }
                    }
                }
            }

            if self.cfg.build_deps {
                if let Some(table) = manifest_toml.get("build-dependencies") {
                    if let Some(table) = table.as_table() {
                        for (dep_name, _) in table.iter() {
                            add_kind(&mut dep_kinds_map, dep_name.to_string(), DepKind::Build);
                        }
                    }
                }
            }

            if self.cfg.dev_deps {
                if let Some(table) = manifest_toml.get("dev-dependencies") {
                    if let Some(table) = table.as_table() {
                        for (dep_name, _) in table.iter() {
                            add_kind(&mut dep_kinds_map, dep_name.to_string(), DepKind::Dev);
                        }
                    }
                }
            }

            root_deps_map.insert(root_name.to_string(), dep_kinds_map);
        }

        Ok((
            root_crates_tomls
                .iter()
                .map(|(root_crate, _)| root_crate.clone())
                .collect(),
            root_deps_map,
        ))
    }

    /// Builds a graph of the resolved dependencies declared in the lock file.
    fn parse_lock_file(
        &self,
        lock_path: PathBuf,
        root_crates: &[RootCrate],
        root_deps_map: RootDepsMap,
    ) -> CliResult<DepGraph> {
        let lock_toml = util::toml_from_file(lock_path)?;

        let mut dg = DepGraph::new(self.cfg.clone());
        dg.root_deps_map = root_deps_map;

        if let Some(root) = lock_toml.get("root") {
            parse_package(&mut dg, root, root_crates)?;
        }

        if let Some(&Value::Array(ref packages)) = lock_toml.get("package") {
            for pkg in packages {
                parse_package(&mut dg, pkg, root_crates)?;
            }
        }

        // Check that all root crates were found in the lock files.
        for &RootCrate { ref name, ref ver } in root_crates.iter() {
            if dg.find(&name, &ver).is_none() {
                return Err(CliError::Toml(format!(
                    "Missing 'name': {} and 'version': {} in lock file",
                    name, ver
                )));
            }
        }

        Ok(dg)
    }
}

fn add_kind(dep_kinds_map: &mut DepKindsMap, key: String, kind: DepKind) {
    let kinds = dep_kinds_map.entry(key).or_insert_with(|| vec![]);
    kinds.push(kind);
}

fn parse_package(dg: &mut DepGraph, pkg: &Value, root_crates: &[RootCrate]) -> CliResult<()> {
    let name = pkg
        .get("name")
        .expect("No 'name' field in Cargo.lock [package] or [root] table")
        .as_str()
        .expect(
            "'name' field of [package] or [root] table in Cargo.lock was not a \
             valid string",
        )
        .to_owned();
    let ver = pkg
        .get("version")
        .expect("No 'version' field in Cargo.lock [package] or [root] table")
        .as_str()
        .expect(
            "'version' field of [package] or [root] table in Cargo.lock was not a \
             valid string",
        )
        .to_owned();

    // If --filter was specified, keep only packages that were indicated.
    let filter = dg.cfg.filter.clone();
    if let Some(ref filter_deps) = filter {
        // NOTE: This will filter out root crates if they are passed in. This is useful for e.g.
        // workspaces if the user does not want all roots.
        if !filter_deps.contains(&name) {
            return Ok(());
        }
    }

    let id = dg.find_or_add(&name, &ver);

    if dg.root_deps_map.contains_key(&name) {
        // If this is a root crate, check that this crate is in `root_crates` with the same version.
        if !root_crates
            .iter()
            .any(|root_crate| root_crate.name == name && root_crate.ver == ver)
        {
            return Err(CliError::Generic(format!(
                "Version {} of root crate '{}' in Cargo.lock does not \
                 match version specified in Cargo.toml",
                ver, name
            )));
        }
    }

    if let Some(&Value::Array(ref deps)) = pkg.get("dependencies") {
        for dep in deps {
            let dep_vec = dep.as_str().unwrap_or("").split(' ').collect::<Vec<_>>();
            let dep_name = dep_vec[0].to_string();
            let dep_ver = dep_vec[1];

            if let Some(ref filter_deps) = filter {
                if !filter_deps.contains(&dep_name) {
                    continue;
                }
            }

            if let Some(dep_kinds_map) = dg.root_deps_map.get(&name) {
                if dep_kinds_map.get(&dep_name).is_none() {
                    // This dep was filtered out when adding root dependencies.
                    continue;
                }
            }

            dg.add_child(id, &dep_name, dep_ver);
        }
    }

    Ok(())
}
