use std::path::{Path, PathBuf};
use std::str::FromStr;

use cargo_toml::Error as CargoTomlError;
use cargo_toml::Manifest;
use rpm::{Compressor, Dependency, RPMBuilder};
use toml::value::Table;

use crate::auto_req::{find_requires, AutoReqMode};
use crate::build_target::BuildTarget;
use crate::error::{ConfigError, Error};
use file_info::FileInfo;
use metadata::{CompoundMetadataConfig, ExtraMetaData, MetadataConfig, TomlValueHelper};

mod file_info;
mod metadata;

#[derive(Debug, Clone)]
pub enum ExtraMetadataSource {
    File(PathBuf, Option<String>),
    Text(String),
}

#[derive(Debug)]
pub struct RpmBuilderConfig<'a, 'b> {
    build_target: &'a BuildTarget,
    auto_req_mode: AutoReqMode,
    payload_compress: &'b str,
}

impl<'a, 'b> RpmBuilderConfig<'a, 'b> {
    pub fn new(
        build_target: &'a BuildTarget,
        auto_req_mode: AutoReqMode,
        payload_compress: &'b str,
    ) -> RpmBuilderConfig<'a, 'b> {
        RpmBuilderConfig {
            build_target,
            auto_req_mode,
            payload_compress,
        }
    }
}

#[derive(Debug)]
pub struct Config {
    manifest: Manifest,
    manifest_path: PathBuf,
    extra_metadata: Vec<ExtraMetaData>,
}

impl Config {
    pub fn new(path: &Path, extra_metadata: &[ExtraMetadataSource]) -> Result<Self, Error> {
        let manifest_path = path.to_path_buf();
        let extra_metadata = extra_metadata
            .iter()
            .map(|v| ExtraMetaData::new(v))
            .collect::<Result<Vec<_>, _>>()?;
        Manifest::from_path(&path)
            .map(|manifest| Config {
                manifest,
                manifest_path,
                extra_metadata,
            })
            .map_err(|err| match err {
                CargoTomlError::Io(e) => Error::FileIo(PathBuf::from(path), e),
                _ => Error::CargoToml(err),
            })
    }

    fn table_to_dependencies(table: &Table) -> Result<Vec<Dependency>, ConfigError> {
        let mut dependencies = Vec::with_capacity(table.len());
        for (key, value) in table {
            let ver = value
                .as_str()
                .ok_or(ConfigError::WrongDependencyVersion(key.clone()))?
                .trim();
            let ver_vec = ver.trim().split_whitespace().collect::<Vec<_>>();
            let dependency = match ver_vec.as_slice() {
                [] | ["*"] => Ok(Dependency::any(key)),
                ["<", ver] => Ok(Dependency::less(key.as_str(), ver.trim())),
                ["<=", ver] => Ok(Dependency::less_eq(key.as_str(), ver.trim())),
                ["=", ver] => Ok(Dependency::eq(key.as_str(), ver.trim())),
                [">", ver] => Ok(Dependency::greater(key.as_str(), ver.trim())),
                [">=", ver] => Ok(Dependency::greater_eq(key.as_str(), ver.trim())),
                _ => Err(ConfigError::WrongDependencyVersion(key.clone())),
            }?;
            dependencies.push(dependency);
        }
        Ok(dependencies)
    }

    pub fn create_rpm_builder(
        &self,
        rpm_builder_config: RpmBuilderConfig,
    ) -> Result<RPMBuilder, Error> {
        let mut metadata_config = Vec::new();
        metadata_config.push(MetadataConfig::new_from_manifest(&self.manifest)?);
        for v in &self.extra_metadata {
            metadata_config.push(MetadataConfig::new_from_extra_metadata(v));
        }
        let metadata = CompoundMetadataConfig::new(metadata_config.as_slice());

        let pkg = self
            .manifest
            .package
            .as_ref()
            .ok_or(ConfigError::Missing("package".to_string()))?;
        let name = metadata
            .get_str("name")?
            .unwrap_or_else(|| pkg.name.as_str());
        let version = metadata
            .get_str("version")?
            .unwrap_or_else(|| pkg.version.as_str());
        let license = metadata
            .get_str("license")?
            .or_else(|| pkg.license.as_ref().map(|v| v.as_ref()))
            .ok_or(ConfigError::Missing("package.license".to_string()))?;
        let arch = rpm_builder_config.build_target.binary_arch();
        let desc = metadata
            .get_str("summary")?
            .or_else(|| pkg.description.as_ref().map(|v| v.as_ref()))
            .ok_or(ConfigError::Missing("package.description".to_string()))?;
        let assets = metadata
            .get_array("assets")?
            .ok_or(ConfigError::Missing("package.assets".to_string()))?;
        let files = FileInfo::new(assets)?;
        let parent = self.manifest_path.parent().unwrap();

        let mut builder = RPMBuilder::new(name, version, license, arch.as_str(), desc)
            .compression(Compressor::from_str(rpm_builder_config.payload_compress)?);
        for file in &files {
            let file_source =
                file.generate_rpm_file_path(rpm_builder_config.build_target, parent)?;
            let options = file.generate_rpm_file_options();
            builder = builder.with_file(file_source, options)?;
        }

        if let Some(release) = metadata.get_string_or_i64("release")? {
            builder = builder.release(release);
        }
        if let Some(epoch) = metadata.get_i64("epoch")? {
            builder = builder.epoch(epoch as i32);
        }

        if let Some(pre_install_script) = metadata.get_str("pre_install_script")? {
            builder = builder.pre_install_script(pre_install_script);
        }
        if let Some(pre_uninstall_script) = metadata.get_str("pre_uninstall_script")? {
            builder = builder.pre_uninstall_script(pre_uninstall_script);
        }
        if let Some(post_install_script) = metadata.get_str("post_install_script")? {
            builder = builder.post_install_script(post_install_script);
        }
        if let Some(post_uninstall_script) = metadata.get_str("post_uninstall_script")? {
            builder = builder.post_uninstall_script(post_uninstall_script);
        }


        if let Some(repository) = match (metadata.get_str("repository")?, pkg.repository.as_ref()) {
            (Some(v), _) => Some(v),
            (None, None) => None,
            (None, Some(v)) => Some(v.as_str()),
        } {
            builder = builder.repository(repository);
        }

        if let Some(vendor) = metadata.get_str("vendor")? {
            builder = builder.vendor(vendor);
        }

        if let Some(requires) = metadata.get_table("requires")? {
            for dependency in Self::table_to_dependencies(requires)? {
                builder = builder.requires(dependency);
            }
        }
        let auto_req = if rpm_builder_config.auto_req_mode == AutoReqMode::Auto
            && matches!(metadata.get_str("auto-req")?, Some("no") | Some("disabled"))
        {
            AutoReqMode::Disabled
        } else {
            rpm_builder_config.auto_req_mode
        };
        for requires in find_requires(files.iter().map(|v| Path::new(&v.source)), auto_req)? {
            builder = builder.requires(Dependency::any(requires));
        }
        if let Some(obsoletes) = metadata.get_table("obsoletes")? {
            for dependency in Self::table_to_dependencies(obsoletes)? {
                builder = builder.obsoletes(dependency);
            }
        }
        if let Some(conflicts) = metadata.get_table("conflicts")? {
            for dependency in Self::table_to_dependencies(conflicts)? {
                builder = builder.conflicts(dependency);
            }
        }
        if let Some(provides) = metadata.get_table("provides")? {
            for dependency in Self::table_to_dependencies(provides)? {
                builder = builder.provides(dependency);
            }
        }

        Ok(builder)
    }
}

#[cfg(test)]
mod test {
    use cargo_toml::Value;

    use super::*;

    #[test]
    fn test_config_new() {
        let config = Config::new(Path::new("Cargo.toml"), &[]).unwrap();
        let pkg = config.manifest.package.unwrap();
        assert_eq!(pkg.name, "cargo-generate-rpm");

        assert!(
            matches!(Config::new(Path::new("not_exist_path/Cargo.toml"), &[]),
            Err(Error::FileIo(path, error)) if path == PathBuf::from("not_exist_path/Cargo.toml") && error.kind() == std::io::ErrorKind::NotFound)
        );
        assert!(matches!(
            Config::new(Path::new("src/error.rs"), &[]),
            Err(Error::CargoToml(_))
        ));
    }

    #[test]
    fn test_new() {
        let config = Config::new(Path::new("Cargo.toml"), &[]).unwrap();
        assert_eq!(config.manifest.package.unwrap().name, "cargo-generate-rpm");
        assert_eq!(config.manifest_path, PathBuf::from("Cargo.toml"));
    }

    #[test]
    fn test_table_to_dependencies() {
        fn dependency_to_u8_slice(dep: &Dependency) -> &[u8] {
            unsafe { std::mem::transmute_copy(dep) }
        }

        let mut table = Table::new();
        [
            ("any1", ""),
            ("any2", "*"),
            ("less", "< 1.0"),
            ("lesseq", "<= 1.0"),
            ("eq", "= 1.0"),
            ("greater", "> 1.0"),
            ("greatereq", "<= 1.0"),
        ]
        .iter()
        .for_each(|(k, v)| {
            table.insert(k.to_string(), Value::String(v.to_string()));
        });

        assert_eq!(
            Config::table_to_dependencies(&table)
                .unwrap()
                .iter()
                .map(&dependency_to_u8_slice)
                .collect::<Vec<_>>(),
            vec![
                dependency_to_u8_slice(&Dependency::any("any1")),
                dependency_to_u8_slice(&Dependency::any("any2")),
                dependency_to_u8_slice(&Dependency::eq("eq", "1.0")),
                dependency_to_u8_slice(&Dependency::greater("greater", "1.0")),
                dependency_to_u8_slice(&Dependency::greater_eq("greatereq", "1.0")),
                dependency_to_u8_slice(&Dependency::less("less", "1.0")),
                dependency_to_u8_slice(&Dependency::less_eq("lesseq", "1.0")),
            ]
        );

        // table.clear();
        table.insert("error".to_string(), Value::Integer(1));
        assert!(matches!(
            Config::table_to_dependencies(&table),
            Err(ConfigError::WrongDependencyVersion(_))
        ));

        table.clear();
        table.insert("error".to_string(), Value::String("1".to_string()));
        assert!(matches!(
            Config::table_to_dependencies(&table),
            Err(ConfigError::WrongDependencyVersion(_))
        ));

        table.clear();
        table.insert("error".to_string(), Value::String("!= 1".to_string()));
        assert!(matches!(
            Config::table_to_dependencies(&table),
            Err(ConfigError::WrongDependencyVersion(_))
        ));

        table.clear();
        table.insert("error".to_string(), Value::String("> 1 1".to_string()));
        assert!(matches!(
            Config::table_to_dependencies(&table),
            Err(ConfigError::WrongDependencyVersion(_))
        ));
    }

    #[test]
    fn test_config_create_rpm_builder() {
        let config = Config::new(Path::new("Cargo.toml"), &[]).unwrap();
        let builder = config.create_rpm_builder(RpmBuilderConfig::new(
            &BuildTarget::default(),
            AutoReqMode::Disabled,
            "zstd",
        ));

        assert!(if Path::new("target/release/cargo-generate-rpm").exists() {
            matches!(builder, Ok(_))
        } else {
            matches!(builder, Err(Error::Config(ConfigError::AssetFileNotFound(path))) if path.to_str() == Some("target/release/cargo-generate-rpm"))
        });
    }
}
