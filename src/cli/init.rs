use std::{
    cmp::PartialEq,
    fs,
    io::{Error, ErrorKind, Write},
    path::{Path, PathBuf},
    str::FromStr,
};

use clap::{Parser, ValueEnum};
use miette::{Context, IntoDiagnostic};
use minijinja::{context, Environment};
use pixi_config::{get_default_author, Config};
use pixi_consts::consts;
use pixi_manifest::{
    pyproject::PyProjectManifest, DependencyOverwriteBehavior, FeatureName, SpecType,
};
use pixi_utils::conda_environment_file::CondaEnvFile;
use rattler_conda_types::{NamedChannelOrUrl, Platform};
use tokio::fs::OpenOptions;
use url::Url;
use uv_normalize::PackageName;

use crate::Project;

#[derive(Parser, Debug, Clone, PartialEq, ValueEnum)]
pub enum ManifestFormat {
    Pixi,
    Pyproject,
}

/// Creates a new project
#[derive(Parser, Debug)]
pub struct Args {
    /// Where to place the project (defaults to current path)
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Channels to use in the project.
    #[arg(short, long = "channel", id = "channel", conflicts_with = "env_file")]
    pub channels: Option<Vec<NamedChannelOrUrl>>,

    /// Platforms that the project supports.
    #[arg(short, long = "platform", id = "platform")]
    pub platforms: Vec<String>,

    /// Environment.yml file to bootstrap the project.
    #[arg(short = 'i', long = "import")]
    pub env_file: Option<PathBuf>,

    /// The manifest format to create.
    #[arg(long, conflicts_with_all = ["env_file", "pyproject_toml"], ignore_case = true)]
    pub format: Option<ManifestFormat>,

    /// Create a pyproject.toml manifest instead of a pixi.toml manifest
    // BREAK (0.27.0): Remove this option from the cli in favor of the `format` option.
    #[arg(long, conflicts_with_all = ["env_file", "format"], alias = "pyproject", hide = true)]
    pub pyproject_toml: bool,

    /// Source Control Management used for this project
    #[arg(short = 's', long = "scm", ignore_case = true)]
    pub scm: Option<GitAttributes>,
}

/// The pixi.toml template
///
/// This uses a template just to simplify the flexibility of emitting it.
const PROJECT_TEMPLATE: &str = r#"[project]
{%- if author %}
authors = ["{{ author[0] }} <{{ author[1] }}>"]
{%- endif %}
channels = {{ channels }}
description = "Add a short description here"
name = "{{ name }}"
platforms = {{ platforms }}
version = "{{ version }}"

{%- if index_url or extra_indexes %}

[pypi-options]
{% if index_url %}index-url = "{{ index_url }}"{% endif %}
{% if extra_index_urls %}extra-index-urls = {{ extra_index_urls }}{% endif %}
{%- endif %}

[tasks]

[dependencies]

"#;

/// The pyproject.toml template
///
/// This is injected into an existing pyproject.toml
const PYROJECT_TEMPLATE_EXISTING: &str = r#"
[tool.pixi.project]
{%- if pixi_name %}
name = "{{ name }}"
{%- endif %}
channels = {{ channels }}
platforms = {{ platforms }}

[tool.pixi.pypi-dependencies]
{{ name }} = { path = ".", editable = true }
{%- for env, features in environments|items %}
{%- if loop.first %}

[tool.pixi.environments]
default = { solve-group = "default" }
{%- endif %}
{{env}} = { features = {{ features }}, solve-group = "default" }
{%- endfor %}

[tool.pixi.tasks]

"#;

/// The pyproject.toml template
///
/// This is used to create a pyproject.toml from scratch
const NEW_PYROJECT_TEMPLATE: &str = r#"[project]
{%- if author %}
authors = [{name = "{{ author[0] }}", email = "{{ author[1] }}"}]
{%- endif %}
dependencies = []
description = "Add a short description here"
name = "{{ name }}"
requires-python = ">= 3.11"
version = "{{ version }}"

[build-system]
build-backend = "hatchling.build"
requires = ["hatchling"]

[tool.pixi.project]
channels = {{ channels }}
platforms = {{ platforms }}


{%- if index_url or extra_indexes %}

[tool.pixi.pypi-options]
{% if index_url %}index-url = "{{ index_url }}"{% endif %}
{% if extra_index_urls %}extra-index-urls = {{ extra_index_urls }}{% endif %}
{%- endif %}

[tool.pixi.pypi-dependencies]
{{ pypi_package_name }} = { path = ".", editable = true }

[tool.pixi.tasks]

"#;

const GITIGNORE_TEMPLATE: &str = r#"
# pixi environments
.pixi
*.egg-info
"#;

#[derive(Parser, Debug, Clone, PartialEq, ValueEnum)]
pub enum GitAttributes {
    Github,
    Gitlab,
    Codeberg,
}

impl GitAttributes {
    fn template(&self) -> &'static str {
        match self {
            GitAttributes::Github | GitAttributes::Codeberg => {
                r#"# SCM syntax highlighting
pixi.lock linguist-language=YAML linguist-generated=true
"#
            }
            GitAttributes::Gitlab => {
                r#"# GitLab syntax highlighting
pixi.lock gitlab-language=yaml gitlab-generated=true
"#
            }
        }
    }
}

pub async fn execute(args: Args) -> miette::Result<()> {
    let env = Environment::new();
    let dir = get_dir(args.path).into_diagnostic()?;
    let pixi_manifest_path = dir.join(consts::PROJECT_MANIFEST);
    let pyproject_manifest_path = dir.join(consts::PYPROJECT_MANIFEST);
    let gitignore_path = dir.join(".gitignore");
    let gitattributes_path = dir.join(".gitattributes");
    let config = Config::load_global();

    // Deprecation warning for the `pyproject` option
    if args.pyproject_toml {
        eprintln!(
            "{}The '{}' option is deprecated and will be removed in the future.\nUse '{}' instead.",
            console::style(console::Emoji("⚠️ ", "")).yellow(),
            console::style("--pyproject").bold().red(),
            console::style("--format pyproject").bold().green(),
        );
    }

    // Fail silently if the directory already exists or cannot be created.
    fs_err::create_dir_all(&dir).ok();

    let default_name = get_name_from_dir(&dir).unwrap_or_else(|_| String::from("new_project"));
    let version = "0.1.0";
    let author = get_default_author();
    let platforms = if args.platforms.is_empty() {
        vec![Platform::current().to_string()]
    } else {
        args.platforms.clone()
    };

    // Create a 'pixi.toml' manifest and populate it by importing a conda
    // environment file
    if let Some(env_file_path) = args.env_file {
        // Check if the 'pixi.toml' file doesn't already exist. We don't want to
        // overwrite it.
        if pixi_manifest_path.is_file() {
            miette::bail!("{} already exists", consts::PROJECT_MANIFEST);
        }

        let env_file = CondaEnvFile::from_path(&env_file_path)?;
        let name = env_file
            .name()
            .unwrap_or(default_name.clone().as_str())
            .to_string();

        // TODO: Improve this:
        //  - Use .condarc as channel config
        let (conda_deps, pypi_deps, channels) = env_file.to_manifest(&config)?;
        let rv = render_project(
            &env,
            name,
            version,
            author.as_ref(),
            channels,
            &platforms,
            None,
            &vec![],
        );
        let mut project = Project::from_str(&pixi_manifest_path, &rv)?;
        let channel_config = project.channel_config();
        for spec in conda_deps {
            project.manifest.add_dependency(
                &spec,
                SpecType::Run,
                // No platforms required as you can't define them in the yaml
                &[],
                &FeatureName::default(),
                DependencyOverwriteBehavior::Overwrite,
                &channel_config,
            )?;
        }
        for requirement in pypi_deps {
            project.manifest.add_pep508_dependency(
                &requirement,
                // No platforms required as you can't define them in the yaml
                &[],
                &FeatureName::default(),
                None,
                DependencyOverwriteBehavior::Overwrite,
                &None,
            )?;
        }
        project.save()?;

        eprintln!(
            "{}Created {}",
            console::style(console::Emoji("✔ ", "")).green(),
            // Canonicalize the path to make it more readable, but if it fails just use the path as
            // is.
            project.manifest_path().display()
        );
    } else {
        let channels = if let Some(channels) = args.channels {
            channels
        } else {
            config.default_channels().to_vec()
        };

        let index_url = config.pypi_config.index_url;
        let extra_index_urls = config.pypi_config.extra_index_urls;

        // Dialog with user to create a 'pyproject.toml' or 'pixi.toml' manifest
        // If nothing is defined but there is a `pyproject.toml` file, ask the user.
        let pyproject = if !pixi_manifest_path.is_file()
            && args.format.is_none()
            && !args.pyproject_toml
            && pyproject_manifest_path.is_file()
        {
            dialoguer::Confirm::new()
                .with_prompt(format!("\nA '{}' file already exists.\nDo you want to extend it with the '{}' configuration?", console::style(consts::PYPROJECT_MANIFEST).bold(), console::style("[tool.pixi]").bold().green()))
                .default(false)
                .show_default(true)
                .interact()
                .into_diagnostic()?
        } else {
            args.format == Some(ManifestFormat::Pyproject) || args.pyproject_toml
        };

        // Inject a tool.pixi.project section into an existing pyproject.toml file if
        // there is one without '[tool.pixi.project]'
        if pyproject && pyproject_manifest_path.is_file() {
            let pyproject = PyProjectManifest::from_path(&pyproject_manifest_path)?;

            // Early exit if 'pyproject.toml' already contains a '[tool.pixi.project]' table
            if pyproject.has_pixi_table() {
                eprintln!(
                    "{}Nothing to do here: 'pyproject.toml' already contains a '[tool.pixi.project]' section.",
                    console::style(console::Emoji("🤔 ", "")).blue(),
                );
                return Ok(());
            }

            let (name, pixi_name) = match pyproject.name() {
                Some(name) => (name, false),
                None => (default_name.as_str(), true),
            };
            let environments = pyproject.environments_from_extras().into_diagnostic()?;
            let rv = env
                .render_named_str(
                    consts::PYPROJECT_MANIFEST,
                    PYROJECT_TEMPLATE_EXISTING,
                    context! {
                        name,
                        pixi_name,
                        channels,
                        platforms,
                        environments,
                    },
                )
                .unwrap();
            if let Err(e) = {
                fs::OpenOptions::new()
                    .append(true)
                    .open(pyproject_manifest_path.clone())
                    .and_then(|mut p| p.write_all(rv.as_bytes()))
            } {
                tracing::warn!(
                    "Warning, couldn't update '{}' because of: {}",
                    pyproject_manifest_path.to_string_lossy(),
                    e
                );
            } else {
                // Inform about the addition of the package itself as an editable dependency of
                // the project
                eprintln!(
                    "{}Added package '{}' as an editable dependency.",
                    console::style(console::Emoji("✔ ", "")).green(),
                    name
                );
                // Inform about the addition of environments from optional dependencies
                // or dependency groups (if any)
                if !environments.is_empty() {
                    let envs: Vec<&str> = environments.keys().map(AsRef::as_ref).collect();
                    eprintln!(
                        "{}Added environment{} '{}' from optional dependencies or dependency groups.",
                        console::style(console::Emoji("✔ ", "")).green(),
                        if envs.len() > 1 { "s" } else { "" },
                        envs.join("', '")
                    )
                }
            }

            // Create a 'pyproject.toml' manifest
        } else if pyproject {
            // Python package names cannot contain '-', so we replace them with '_'
            let pypi_package_name = PackageName::from_str(&default_name)
                .map(|name| name.as_dist_info_name().to_string())
                .unwrap_or_else(|_| default_name.clone());

            let rv = env
                .render_named_str(
                    consts::PYPROJECT_MANIFEST,
                    NEW_PYROJECT_TEMPLATE,
                    context! {
                        name => default_name,
                        pypi_package_name,
                        version,
                        author,
                        channels,
                        platforms,
                        index_url => index_url.as_ref(),
                        extra_index_urls => &extra_index_urls,
                    },
                )
                .unwrap();
            save_manifest_file(&pyproject_manifest_path, rv)?;

            let src_dir = dir.join("src").join(pypi_package_name);
            tokio::fs::create_dir_all(&src_dir)
                .await
                .into_diagnostic()
                .wrap_err_with(|| format!("Could not create directory {}.", src_dir.display()))?;

            let init_file = src_dir.join("__init__.py");
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&init_file)
                .await
            {
                Ok(_) => (),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    // If the file already exists, do nothing
                }
                Err(e) => {
                    return Err(e).into_diagnostic().wrap_err_with(|| {
                        format!("Could not create file {}.", init_file.display())
                    });
                }
            };

        // Create a 'pixi.toml' manifest
        } else {
            // Check if the 'pixi.toml' file doesn't already exist. We don't want to
            // overwrite it.
            if pixi_manifest_path.is_file() {
                miette::bail!("{} already exists", consts::PROJECT_MANIFEST);
            }
            let rv = render_project(
                &env,
                default_name,
                version,
                author.as_ref(),
                channels,
                &platforms,
                index_url.as_ref(),
                &extra_index_urls,
            );
            save_manifest_file(&pixi_manifest_path, rv)?;
        };
    }

    // create a .gitignore if one is missing
    if let Err(e) = create_or_append_file(&gitignore_path, GITIGNORE_TEMPLATE) {
        tracing::warn!(
            "Warning, couldn't update '{}' because of: {}",
            gitignore_path.to_string_lossy(),
            e
        );
    }

    let git_attributes = args.scm.unwrap_or(GitAttributes::Github);

    // create a .gitattributes if one is missing
    if let Err(e) = create_or_append_file(&gitattributes_path, git_attributes.template()) {
        tracing::warn!(
            "Warning, couldn't update '{}' because of: {}",
            gitattributes_path.to_string_lossy(),
            e
        );
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_project(
    env: &Environment<'_>,
    name: String,
    version: &str,
    author: Option<&(String, String)>,
    channels: Vec<NamedChannelOrUrl>,
    platforms: &Vec<String>,
    index_url: Option<&Url>,
    extra_index_urls: &Vec<Url>,
) -> String {
    env.render_named_str(
        consts::PROJECT_MANIFEST,
        PROJECT_TEMPLATE,
        context! {
            name,
            version,
            author,
            channels,
            platforms,
            index_url,
            extra_index_urls,
        },
    )
    .unwrap()
}

/// Save the rendered template to a file, and print a message to the user.
fn save_manifest_file(path: &Path, content: String) -> miette::Result<()> {
    fs_err::write(path, content).into_diagnostic()?;
    eprintln!(
        "{}Created {}",
        console::style(console::Emoji("✔ ", "")).green(),
        // Canonicalize the path to make it more readable, but if it fails just use the path as is.
        dunce::canonicalize(path)
            .unwrap_or(path.to_path_buf())
            .display()
    );
    Ok(())
}

fn get_name_from_dir(path: &Path) -> miette::Result<String> {
    Ok(path
        .file_name()
        .ok_or(miette::miette!(
            "Cannot get file or directory name from the path: {}",
            path.to_string_lossy()
        ))?
        .to_string_lossy()
        .to_string())
}

// When the specific template is not in the file or the file does not exist.
// Make the file and append the template to the file.
fn create_or_append_file(path: &Path, template: &str) -> std::io::Result<()> {
    let file = fs_err::read_to_string(path).unwrap_or_default();

    if !file.contains(template) {
        fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)?
            .write_all(template.as_bytes())?;
    }
    Ok(())
}

fn get_dir(path: PathBuf) -> Result<PathBuf, Error> {
    if path.components().count() == 1 {
        Ok(std::env::current_dir().unwrap_or_default().join(path))
    } else {
        path.canonicalize().map_err(|e| match e.kind() {
            ErrorKind::NotFound => Error::new(
                ErrorKind::NotFound,
                format!(
                    "Cannot find '{}' please make sure the folder is reachable",
                    path.to_string_lossy()
                ),
            ),
            _ => Error::new(
                ErrorKind::InvalidInput,
                "Cannot canonicalize the given path",
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Read,
        path::{Path, PathBuf},
    };

    use tempfile::tempdir;

    use super::*;
    use crate::cli::init::get_dir;

    #[test]
    fn test_get_name() {
        assert_eq!(
            get_dir(PathBuf::from(".")).unwrap(),
            std::env::current_dir().unwrap()
        );
        assert_eq!(
            get_dir(PathBuf::from("test_folder")).unwrap(),
            std::env::current_dir().unwrap().join("test_folder")
        );
        assert_eq!(
            get_dir(std::env::current_dir().unwrap()).unwrap(),
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
    }

    #[test]
    fn test_get_name_panic() {
        match get_dir(PathBuf::from("invalid/path")) {
            Ok(_) => panic!("Expected error, but got OK"),
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
        }
    }

    #[test]
    fn test_create_or_append_file() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test_file.txt");
        let template = "Test Template";

        fn read_file_content(path: &Path) -> String {
            let mut file = fs_err::File::open(path).unwrap();
            let mut content = String::new();
            file.read_to_string(&mut content).unwrap();
            content
        }

        // Scenario 1: File does not exist.
        create_or_append_file(&file_path, template).unwrap();
        assert_eq!(read_file_content(&file_path), template);

        // Scenario 2: File exists but doesn't contain the template.
        create_or_append_file(&file_path, "New Content").unwrap();
        assert!(read_file_content(&file_path).contains(template));
        assert!(read_file_content(&file_path).contains("New Content"));

        // Scenario 3: File exists and already contains the template.
        let original_content = read_file_content(&file_path);
        create_or_append_file(&file_path, template).unwrap();
        assert_eq!(read_file_content(&file_path), original_content);

        // Scenario 4: Path is a folder not a file, give an error.
        assert!(create_or_append_file(dir.path(), template).is_err());

        dir.close().unwrap();
    }

    #[test]
    fn test_multiple_format_values() {
        let test_cases = vec![
            ("pixi", ManifestFormat::Pixi),
            ("PiXi", ManifestFormat::Pixi),
            ("PIXI", ManifestFormat::Pixi),
            ("pyproject", ManifestFormat::Pyproject),
            ("PyPrOjEcT", ManifestFormat::Pyproject),
            ("PYPROJECT", ManifestFormat::Pyproject),
        ];

        for (input, expected) in test_cases {
            let args = Args::try_parse_from(["init", "--format", input]).unwrap();
            assert_eq!(args.format, Some(expected));
        }
    }

    #[test]
    fn test_multiple_scm_values() {
        let test_cases = vec![
            ("github", GitAttributes::Github),
            ("GiThUb", GitAttributes::Github),
            ("GITHUB", GitAttributes::Github),
            ("Github", GitAttributes::Github),
            ("gitlab", GitAttributes::Gitlab),
            ("GiTlAb", GitAttributes::Gitlab),
            ("GITLAB", GitAttributes::Gitlab),
            ("codeberg", GitAttributes::Codeberg),
            ("CoDeBeRg", GitAttributes::Codeberg),
            ("CODEBERG", GitAttributes::Codeberg),
        ];

        for (input, expected) in test_cases {
            let args = Args::try_parse_from(["init", "--scm", input]).unwrap();
            assert_eq!(args.scm, Some(expected));
        }
    }

    #[test]
    fn test_invalid_scm_values() {
        let invalid_values = vec!["invalid", "", "git", "bitbucket", "mercurial", "svn"];

        for value in invalid_values {
            let result = Args::try_parse_from(["init", "--scm", value]);
            assert!(
                result.is_err(),
                "Expected error for invalid SCM value '{}', but got success",
                value
            );
        }
    }
}
