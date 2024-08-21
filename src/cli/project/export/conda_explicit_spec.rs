use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;

use crate::cli::cli_config::PrefixUpdateConfig;
use crate::cli::LockFileUsageArgs;
use crate::lock_file::UpdateLockFileOptions;
use crate::Project;
use rattler_conda_types::{ExplicitEnvironmentEntry, ExplicitEnvironmentSpec, Platform};
use rattler_lock::{CondaPackage, Package, PackageHashes, PypiPackage, PypiPackageData, UrlOrPath};

#[derive(Debug, Parser)]
#[clap(arg_required_else_help = false)]
pub struct Args {
    /// Environment to render
    #[arg(short, long)]
    environment: Option<String>,

    /// The platform to render. Defaults to the current platform.
    #[arg(long)]
    pub platform: Option<Platform>,

    /// PyPI dependencies are not supported in the conda spec file.
    /// This flag allows creating the spec file even if PyPI dependencies are present.
    /// Alternatively see --write-pypi-requirements
    #[arg(long, default_value = "false")]
    ignore_pypi_errors: bool,

    /// Write a requirements file containing all pypi dependencies
    #[arg(long, default_value = "false", conflicts_with = "ignore_pypi_errors")]
    write_pypi_requirements: bool,

    #[clap(flatten)]
    pub lock_file_usage: LockFileUsageArgs,

    #[clap(flatten)]
    pub prefix_update_config: PrefixUpdateConfig,
}

fn cwd() -> PathBuf {
    std::env::current_dir().expect("failed to obtain current working directory")
}

fn build_explicit_spec<'a>(
    platform: Platform,
    conda_packages: impl IntoIterator<Item = &'a CondaPackage>,
) -> miette::Result<ExplicitEnvironmentSpec> {
    let mut packages = Vec::new();

    for cp in conda_packages {
        let prec = cp.package_record();
        let mut url = cp.url().to_owned();
        let hash = prec.md5.ok_or(miette::miette!(
            "Package {} does not contain an md5 hash",
            prec.name.as_normalized()
        ))?;

        url.set_fragment(Some(&format!("{:x}", hash)));

        packages.push(ExplicitEnvironmentEntry {
            url: url.to_owned(),
        });
    }

    Ok(ExplicitEnvironmentSpec {
        platform: Some(platform),
        packages,
    })
}

fn write_explicit_spec(
    target: impl AsRef<Path>,
    exp_env_spec: &ExplicitEnvironmentSpec,
) -> miette::Result<()> {
    let mut environment = String::new();
    environment.push_str("# Generated by `pixi project export`\n");
    environment.push_str(exp_env_spec.to_spec_string().as_str());

    fs::write(target, environment)
        .map_err(|e| miette::miette!("Could not write environment file: {}", e))?;

    Ok(())
}

fn get_pypi_hash_str(package_data: &PypiPackageData) -> Option<String> {
    if let Some(hashes) = &package_data.hash {
        let h = match hashes {
            PackageHashes::Sha256(h) => format!("--hash=sha256:{:x}", h).to_string(),
            PackageHashes::Md5Sha256(_, h) => format!("--hash=sha256:{:x}", h).to_string(),
            PackageHashes::Md5(h) => format!("--hash=md5:{:x}", h).to_string(),
        };
        Some(h)
    } else {
        None
    }
}

fn write_pypi_requirements(
    target: impl AsRef<Path>,
    packages: &[PypiPackage],
) -> miette::Result<()> {
    let mut reqs = String::new();

    for p in packages {
        // pip --verify-hashes does not accept hashes for local files
        let (s, include_hash) = match p.url() {
            UrlOrPath::Url(url) => (url.as_str(), true),
            UrlOrPath::Path(path) => (
                path.as_os_str()
                    .to_str()
                    .unwrap_or_else(|| panic!("Could not convert {:?} to str", path)),
                false,
            ),
        };

        // remove "direct+ since not valid for pip urls"
        let s = s.trim_start_matches("direct+");

        let hash = match (include_hash, get_pypi_hash_str(p.data().package)) {
            (true, Some(h)) => format!(" {}", h),
            (false, _) => "".to_string(),
            (_, None) => "".to_string(),
        };

        if p.is_editable() {
            reqs.push_str(&format!("-e {}{}\n", s, hash));
        } else {
            reqs.push_str(&format!("{}{}\n", s, hash));
        }
    }

    fs::write(target, reqs)
        .map_err(|e| miette::miette!("Could not write requirements file: {}", e))?;

    Ok(())
}

pub async fn execute(project: Project, args: Args) -> miette::Result<()> {
    let environment = project.environment_from_name_or_env_var(args.environment)?;
    // Load the platform
    let platform = args.platform.unwrap_or_else(|| environment.best_platform());

    let lock_file = project
        .update_lock_file(UpdateLockFileOptions {
            lock_file_usage: args.prefix_update_config.lock_file_usage(),
            no_install: args.prefix_update_config.no_install,
            ..UpdateLockFileOptions::default()
        })
        .await?
        .lock_file;

    let env = lock_file
        .environment(environment.name().as_str())
        .ok_or(miette::miette!(
            "unknown environment '{}' in {}",
            environment.name(),
            project
                .manifest_path()
                .to_str()
                .expect("expected to have a manifest_path")
        ))?;

    let packages = env.packages(platform).ok_or(miette::miette!(
        "platform '{platform}' not found in {}",
        project
            .manifest_path()
            .to_str()
            .expect("expected to have a manifest_path"),
    ))?;

    let mut conda_packages_from_lockfile: Vec<CondaPackage> = Vec::new();
    let mut pypi_packages_from_lockfile: Vec<PypiPackage> = Vec::new();

    for package in packages {
        match package {
            Package::Conda(p) => conda_packages_from_lockfile.push(p),
            Package::Pypi(pyp) => {
                if args.ignore_pypi_errors {
                    tracing::warn!("ignoring PyPI package since PyPI packages are not supported");
                } else if args.write_pypi_requirements {
                    pypi_packages_from_lockfile.push(pyp);
                } else {
                    miette::bail!(
                        "PyPI packages are not supported. Specify `--ignore-pypi-errors` to ignore this error\
                        or `--write-pypi-requirements` to write pypi requirements to a separate requirements.txt file"
                    );
                }
            }
        }
    }

    let ees = build_explicit_spec(platform, &conda_packages_from_lockfile)?;

    tracing::info!("Creating conda lock file");
    let target = cwd()
        .join(format!(
            "conda-{}-{}.lock",
            platform,
            environment.name().as_str()
        ))
        .into_os_string();

    write_explicit_spec(target, &ees)?;

    if args.write_pypi_requirements {
        tracing::info!("Creating conda lock file");
        let pypi_target = cwd()
            .join(format!(
                "requirements-{}-{}.txt",
                platform,
                environment.name().as_str()
            ))
            .into_os_string();

        write_pypi_requirements(pypi_target, &pypi_packages_from_lockfile)?;
    }

    Ok(())
}
