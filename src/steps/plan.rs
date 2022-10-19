use std::path::Path;
use std::path::PathBuf;

use crate::config;
use crate::error::CargoResult;
use crate::ops::cargo;
use crate::ops::git;
use crate::ops::replace::Template;
use crate::ops::version::VersionExt as _;

pub fn load(
    args: &config::ConfigArgs,
    ws_meta: &cargo_metadata::Metadata,
) -> CargoResult<indexmap::IndexMap<cargo_metadata::PackageId, PackageRelease>> {
    let root = git::top_level(ws_meta.workspace_root.as_std_path())?;

    let member_ids = cargo::sort_workspace(ws_meta);
    member_ids
        .iter()
        .map(|p| PackageRelease::load(args, &root, ws_meta, &ws_meta[p]))
        .map(|p| p.map(|p| (p.meta.id.clone(), p)))
        .collect()
}

pub fn plan(
    mut pkgs: indexmap::IndexMap<cargo_metadata::PackageId, PackageRelease>,
) -> CargoResult<indexmap::IndexMap<cargo_metadata::PackageId, PackageRelease>> {
    let mut shared_versions: std::collections::HashMap<String, Version> = Default::default();
    for pkg in pkgs.values() {
        let group_name = if let Some(group_name) = pkg.config.shared_version() {
            group_name.to_owned()
        } else {
            continue;
        };
        let version = pkg.planned_version.as_ref().unwrap_or(&pkg.initial_version);
        match shared_versions.entry(group_name) {
            std::collections::hash_map::Entry::Occupied(mut existing) => {
                if existing.get().full_version < version.full_version {
                    existing.insert(version.clone());
                }
            }
            std::collections::hash_map::Entry::Vacant(vacant) => {
                vacant.insert(version.clone());
            }
        }
    }
    if !shared_versions.is_empty() {
        for pkg in pkgs.values_mut() {
            let group_name = if let Some(group_name) = pkg.config.shared_version() {
                group_name
            } else {
                continue;
            };
            let shared_max = shared_versions.get(group_name).unwrap();
            if pkg.initial_version.bare_version != shared_max.bare_version {
                pkg.planned_version = Some(shared_max.clone());
            } else {
                pkg.planned_version = None;
            }
        }
    }

    for pkg in pkgs.values_mut() {
        pkg.plan()?;
    }

    Ok(pkgs)
}

pub struct PackageRelease {
    pub meta: cargo_metadata::Package,
    pub manifest_path: PathBuf,
    pub package_root: PathBuf,
    pub is_root: bool,
    pub config: config::Config,

    pub package_content: Vec<PathBuf>,
    pub bin: bool,
    pub dependents: Vec<Dependency>,
    pub features: cargo::Features,

    pub initial_version: Version,
    pub initial_tag: String,
    pub prior_tag: Option<String>,

    pub planned_version: Option<Version>,
    pub planned_tag: Option<String>,

    pub ensure_owners: bool,
}

impl PackageRelease {
    pub fn load(
        args: &config::ConfigArgs,
        git_root: &Path,
        ws_meta: &cargo_metadata::Metadata,
        pkg_meta: &cargo_metadata::Package,
    ) -> CargoResult<Self> {
        let meta = pkg_meta.clone();
        let manifest_path = pkg_meta.manifest_path.as_std_path().to_owned();
        let package_root = manifest_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_owned();
        let config = config::load_package_config(args, ws_meta, pkg_meta)?;
        if !config.release() {
            log::trace!("Disabled in config, skipping {}", manifest_path.display());
        }

        let package_content = cargo::package_content(&manifest_path)?;
        let bin = pkg_meta
            .targets
            .iter()
            .flat_map(|t| t.kind.iter())
            .any(|k| k == "bin");
        let features = config.features();
        let dependents = find_dependents(ws_meta, pkg_meta)
            .map(|(pkg, dep)| Dependency {
                pkg: pkg.clone(),
                req: dep.req.clone(),
            })
            .collect();

        let is_root = git_root == package_root;
        let initial_version = Version::from(pkg_meta.version.clone());
        let tag_name = config.tag_name();
        let tag_prefix = config.tag_prefix(is_root);
        let name = pkg_meta.name.as_str();
        let initial_tag = render_tag(
            tag_name,
            tag_prefix,
            name,
            &initial_version,
            &initial_version,
        );

        let prior_tag = None;

        let planned_version = None;
        let planned_tag = None;
        let ensure_owners = config.publish() && !config.owners().is_empty();

        let pkg = PackageRelease {
            meta,
            manifest_path,
            package_root,
            is_root,
            config,

            package_content,
            bin,
            dependents,
            features,

            initial_version,
            initial_tag,
            prior_tag,

            planned_version,
            planned_tag,
            ensure_owners,
        };
        Ok(pkg)
    }

    pub fn set_prior_tag(&mut self, prior_tag: String) {
        self.prior_tag = Some(prior_tag);
    }

    pub fn bump(
        &mut self,
        level_or_version: &super::TargetVersion,
        metadata: Option<&str>,
    ) -> CargoResult<()> {
        self.planned_version =
            level_or_version.bump(&self.initial_version.full_version, metadata)?;
        Ok(())
    }

    pub fn plan(&mut self) -> CargoResult<()> {
        if !self.config.release() {
            return Ok(());
        }

        if self.planned_version.is_some()
            && crate::ops::git::tag_exists(&self.package_root, &self.initial_tag)?
        {
            self.prior_tag
                .get_or_insert_with(|| self.initial_tag.clone());
        }
        if self.prior_tag.is_none() {
            let tag_name = self.config.tag_name();
            let tag_prefix = self.config.tag_prefix(self.is_root);
            let name = self.meta.name.as_str();
            let tag_glob = render_tag_glob(tag_name, tag_prefix, name);
            match globset::Glob::new(&tag_glob) {
                Ok(tag_glob) => {
                    let tag_glob = tag_glob.compile_matcher();
                    self.prior_tag = crate::ops::git::find_last_tag(&self.package_root, &tag_glob);
                }
                Err(err) => {
                    log::debug!("Failed to find tag with glob `{}`: {}", tag_glob, err);
                }
            }
        }

        let base = self
            .planned_version
            .as_ref()
            .unwrap_or(&self.initial_version);
        let tag = if self.config.tag() {
            let tag_name = self.config.tag_name();
            let tag_prefix = self.config.tag_prefix(self.is_root);
            let name = self.meta.name.as_str();
            Some(render_tag(
                tag_name,
                tag_prefix,
                name,
                &self.initial_version,
                base,
            ))
        } else {
            None
        };

        self.planned_tag = tag;

        Ok(())
    }
}

fn render_tag(
    tag_name: &str,
    tag_prefix: &str,
    name: &str,
    prev: &Version,
    base: &Version,
) -> String {
    let initial_version_var = prev.bare_version_string.as_str();
    let existing_metadata_var = prev.full_version.build.as_str();
    let version_var = base.bare_version_string.as_str();
    let metadata_var = base.full_version.build.as_str();
    let mut template = Template {
        prev_version: Some(initial_version_var),
        prev_metadata: Some(existing_metadata_var),
        version: Some(version_var),
        metadata: Some(metadata_var),
        crate_name: Some(name),
        ..Default::default()
    };

    let tag_prefix = template.render(tag_prefix);
    template.prefix = Some(&tag_prefix);
    template.render(tag_name)
}

fn render_tag_glob(tag_name: &str, tag_prefix: &str, name: &str) -> String {
    let initial_version_var = "*";
    let existing_metadata_var = "*";
    let version_var = "*";
    let metadata_var = "*";
    let mut template = Template {
        prev_version: Some(initial_version_var),
        prev_metadata: Some(existing_metadata_var),
        version: Some(version_var),
        metadata: Some(metadata_var),
        crate_name: Some(name),
        ..Default::default()
    };

    let tag_prefix = template.render(tag_prefix);
    template.prefix = Some(&tag_prefix);
    template.render(tag_name)
}

fn find_dependents<'w>(
    ws_meta: &'w cargo_metadata::Metadata,
    pkg_meta: &'w cargo_metadata::Package,
) -> impl Iterator<Item = (&'w cargo_metadata::Package, &'w cargo_metadata::Dependency)> {
    ws_meta.packages.iter().filter_map(move |p| {
        if ws_meta.workspace_members.iter().any(|m| *m == p.id) {
            p.dependencies
                .iter()
                .find(|d| d.name == pkg_meta.name)
                .map(|d| (p, d))
        } else {
            None
        }
    })
}

pub struct Dependency {
    pub pkg: cargo_metadata::Package,
    pub req: semver::VersionReq,
}

#[derive(Debug, Clone)]
pub struct Version {
    pub full_version: semver::Version,
    pub full_version_string: String,
    pub bare_version: semver::Version,
    pub bare_version_string: String,
}

impl Version {
    pub fn is_prerelease(&self) -> bool {
        self.full_version.is_prerelease()
    }
}

impl From<semver::Version> for Version {
    fn from(full_version: semver::Version) -> Self {
        let full_version_string = full_version.to_string();
        let mut bare_version = full_version.clone();
        bare_version.build = semver::BuildMetadata::EMPTY;
        let bare_version_string = bare_version.to_string();
        Self {
            full_version,
            full_version_string,
            bare_version,
            bare_version_string,
        }
    }
}
