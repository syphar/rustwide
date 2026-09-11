use super::CrateTrait;
use crate::Workspace;
use anyhow::Context as _;
use flate2::read::GzDecoder;
use log::info;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use tar::Archive;
use url::Url;

pub(crate) static CRATES_IO_SPARSE_INDEX: LazyLock<Url> = LazyLock::new(|| {
    Url::parse("https://index.crates.io/").expect("crates.io sparse index URL is valid")
});

pub(super) fn normalize_sparse_index(mut index: Url) -> anyhow::Result<Url> {
    if let Some(index_url) = index.as_str().strip_prefix("sparse+") {
        index = Url::parse(index_url).context("invalid sparse index URL")?;
    }

    if !index.path().ends_with('/') {
        let path = format!("{}/", index.path());
        index.set_path(&path);
    }
    Ok(index)
}

/// A Git-indexed registry as described in rust-lang/rfcs#2141.
///
/// For an HTTP sparse index, use [`Crate::sparse_registry`](super::Crate::sparse_registry).
#[cfg(feature = "git-registries")]
pub struct GitRegistry {
    registry_index: String,
    key: Option<String>,
}

#[cfg(feature = "git-registries")]
impl GitRegistry {
    /// Create a Git-indexed registry for the specified registry index URL.
    pub fn new(registry_index: impl Into<String>) -> GitRegistry {
        GitRegistry {
            registry_index: registry_index.into(),
            key: None,
        }
    }

    /// Specify private ssh key for registry authentication.
    pub fn authenticate_with_ssh_key(&mut self, key: impl Into<String>) {
        self.key = Some(key.into());
    }

    fn index(&self) -> &str {
        self.registry_index.as_str()
    }

    fn index_folder(&self) -> String {
        crate::utils::escape_path(self.registry_index.as_bytes())
    }
}

pub(crate) enum Registry {
    Sparse(Url),
    #[cfg(feature = "git-registries")]
    Git(GitRegistry),
}

impl Registry {
    fn cache_folder(&self) -> String {
        match self {
            Registry::Sparse(index) if index == &*CRATES_IO_SPARSE_INDEX => {
                "cratesio-sources".into()
            }
            Registry::Sparse(index) => {
                format!(
                    "{}-sources",
                    crate::utils::escape_path(index.as_str().as_bytes())
                )
            }
            #[cfg(feature = "git-registries")]
            Registry::Git(registry) => format!("{}-sources", registry.index_folder()),
        }
    }

    fn name(&self) -> String {
        match self {
            Registry::Sparse(index) if index == &*CRATES_IO_SPARSE_INDEX => "crates.io".into(),
            Registry::Sparse(index) => index.as_str().into(),
            #[cfg(feature = "git-registries")]
            Registry::Git(registry) => registry.index().to_string(),
        }
    }
}

pub(super) struct RegistryCrate {
    registry: Registry,
    name: String,
    version: String,
}

#[derive(serde::Deserialize)]
struct IndexConfig {
    dl: String,
}

impl RegistryCrate {
    pub(super) fn new(registry: Registry, name: &str, version: &str) -> Self {
        RegistryCrate {
            registry,
            name: name.into(),
            version: version.into(),
        }
    }

    fn cache_path(&self, workspace: &Workspace) -> PathBuf {
        workspace
            .cache_dir()
            .join(self.registry.cache_folder())
            .join(&self.name)
            .join(format!("{}-{}.crate", self.name, self.version))
    }

    fn sparse_config(&self, workspace: &Workspace, index: &Url) -> anyhow::Result<IndexConfig> {
        sparse_config(&workspace.cache_dir(), workspace.http_client(), index)
    }
}

/// Generate the path where we cache `config.json` from the given sparse index.
fn sparse_config_path(cache_dir: &Path, index: &Url) -> PathBuf {
    cache_dir
        .join("registry-index")
        .join(crate::utils::escape_path(index.as_str().as_bytes()))
        .join("config.json")
}

/// Fetch & locally cache the `/config.json` file of the given sparse index.
fn sparse_config(
    cache_dir: &Path,
    http_client: &attohttpc::Session,
    index: &Url,
) -> anyhow::Result<IndexConfig> {
    let path = sparse_config_path(cache_dir, index);
    match fs::read_to_string(&path) {
        Ok(config) => serde_json::from_str(&config).context("registry has invalid config.json"),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let config_url = index.join("config.json")?;
            let config = http_client
                .get(config_url.as_str())
                .send()?
                .error_for_status()?
                .text()
                .with_context(|| {
                    format!("unable to fetch sparse registry config at {config_url}")
                })?;

            let parsed = serde_json::from_str::<IndexConfig>(&config)
                .context("registry has invalid config.json")?;

            let parent = path.parent().expect("config path has a parent");
            fs::create_dir_all(parent)?;

            // Write config.json to a temporary path first, then atomically move it into place.
            let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
            temporary.write_all(config.as_bytes())?;
            temporary.persist(&path).map_err(|error| error.error)?;
            Ok(parsed)
        }
        Err(err) => Err(err.into()),
    }
}

impl RegistryCrate {
    #[allow(unused_variables)]
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all, level = "debug"))]
    fn fetch_url(&self, workspace: &Workspace) -> anyhow::Result<Url> {
        match &self.registry {
            Registry::Sparse(index) => {
                let config = self.sparse_config(workspace, index)?;
                download_url(&config.dl, &self.name, &self.version)
            }
            #[cfg(feature = "git-registries")]
            Registry::Git(registry) => {
                let index_path = workspace
                    .cache_dir()
                    .join("registry-index")
                    .join(registry.index_folder());
                if !index_path.exists() {
                    let url = registry.index();
                    let mut fo = git2::FetchOptions::new();
                    if let Some(key) = registry.key.as_deref() {
                        fo.remote_callbacks({
                            let mut callbacks = git2::RemoteCallbacks::new();
                            callbacks.credentials(
                                move |_url, username_from_url, _allowed_types| {
                                    git2::Cred::ssh_key_from_memory(
                                        username_from_url.unwrap(),
                                        None,
                                        key,
                                        None,
                                    )
                                },
                            );
                            callbacks
                        });
                    }

                    git2::build::RepoBuilder::new()
                        .fetch_options(fo)
                        .clone(url, &index_path)
                        .with_context(|| format!("unable to update_index at {url}"))?;
                    info!("cloned registry index");
                }
                let config = std::fs::read_to_string(index_path.join("config.json"))?;
                let config = serde_json::from_str::<IndexConfig>(&config)
                    .context("registry has invalid config.json")?;

                download_url(&config.dl, &self.name, &self.version)
            }
        }
    }
}

/// Generate a download url from a `dl` URL template.
///
/// Replacements are incomplete for now and support only simple use-cases.
/// See https://doc.rust-lang.org/cargo/reference/registry-index.html
fn download_url(template: &str, name: &str, version: &str) -> anyhow::Result<Url> {
    let replacements = [("{crate}", name), ("{version}", version)];
    if !replacements
        .iter()
        .any(|(marker, _)| template.contains(marker))
    {
        Ok(format!("{}/{}/{}/download", template, name, version).parse()?)
    } else {
        Ok(replacements
            .into_iter()
            .fold(template.to_string(), |url, (marker, value)| {
                url.replace(marker, value)
            })
            .parse()?)
    }
}

impl CrateTrait for RegistryCrate {
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            skip_all,
            level = "debug",
            fields(cache_hit = tracing::field::Empty)
        )
    )]
    fn fetch(&self, workspace: &Workspace) -> anyhow::Result<()> {
        let local = self.cache_path(workspace);
        let cache_hit = local.exists();
        #[cfg(feature = "tracing")]
        tracing::Span::current().record("cache_hit", cache_hit);

        if cache_hit {
            info!("crate {} {} is already in cache", self.name, self.version);
            return Ok(());
        }

        info!("fetching crate {} {}...", self.name, self.version);
        if let Some(parent) = local.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let url = self.fetch_url(workspace)?;

        workspace
            .http_client()
            .get(url)
            .send()?
            .error_for_status()?
            .write_to(&mut BufWriter::new(File::create(&local)?))?;

        Ok(())
    }

    fn purge_from_cache(&self, workspace: &Workspace) -> anyhow::Result<()> {
        let path = self.cache_path(workspace);
        if path.exists() {
            crate::utils::remove_file(&path)?;
        }
        Ok(())
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all, level = "debug"))]
    fn copy_source_to(&self, workspace: &Workspace, dest: &Path) -> anyhow::Result<()> {
        let cached = self.cache_path(workspace);
        let mut file = File::open(cached)?;
        let mut tar = Archive::new(GzDecoder::new(BufReader::new(&mut file)));

        info!(
            "extracting crate {} {} into {}",
            self.name,
            self.version,
            dest.display()
        );
        let unpack_result = unpack_without_first_dir(&mut tar, dest);

        if let Err(err) = unpack_result {
            let _ = crate::utils::remove_dir_all(dest);
            Err(err.context(format!(
                "unable to download {} version {}",
                self.name, self.version
            )))
        } else {
            Ok(())
        }
    }
}

impl std::fmt::Display for RegistryCrate {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{} crate {} {}",
            self.registry.name(),
            self.name,
            self.version
        )
    }
}

fn unpack_without_first_dir<R: Read>(archive: &mut Archive<R>, path: &Path) -> anyhow::Result<()> {
    let entries = archive.entries()?;
    for entry in entries {
        let mut entry = entry?;
        let relpath = {
            let path = entry.path();
            let path = path?;
            path.into_owned()
        };
        let mut components = relpath.components();
        // Throw away the first path component
        components.next();
        let full_path = path.join(components.as_path());
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entry.unpack(&full_path)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{download_url, normalize_sparse_index, sparse_config, sparse_config_path};
    use crate::crates::registry::CRATES_IO_SPARSE_INDEX;
    use mockito::Server;
    use std::fs;
    use url::Url;

    #[test]
    fn fetches_sparse_config_once_and_caches_it_by_normalized_index_url() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/index/config.json")
            .with_status(200)
            .with_body(r#"{"dl":"https://downloads.example"}"#)
            .expect(1)
            .create();
        let cache = tempfile::tempdir().unwrap();
        let index = normalize_sparse_index(Url::parse(&format!("{}/index", server.url())).unwrap())
            .unwrap();
        let client = attohttpc::Session::new();
        let first = sparse_config(cache.path(), &client, &index).unwrap();
        let second = sparse_config(cache.path(), &client, &index).unwrap();

        assert_eq!(first.dl, "https://downloads.example");
        assert_eq!(second.dl, "https://downloads.example");
        mock.assert();
        assert_eq!(
            fs::read_to_string(sparse_config_path(cache.path(), &index)).unwrap(),
            r#"{"dl":"https://downloads.example"}"#
        );
    }

    #[test]
    fn invalid_sparse_config_is_not_cached_and_can_be_retried() {
        for invalid_body in ["not JSON", r#"{"api":"https://registry.example"}"#] {
            let mut server = Server::new();
            let cache = tempfile::tempdir().unwrap();
            let index =
                normalize_sparse_index(Url::parse(&format!("{}/index", server.url())).unwrap())
                    .unwrap();
            let client = attohttpc::Session::new();
            let invalid = server
                .mock("GET", "/index/config.json")
                .with_status(200)
                .with_body(invalid_body)
                .expect(1)
                .create();

            assert!(sparse_config(cache.path(), &client, &index).is_err());
            assert!(!sparse_config_path(cache.path(), &index).exists());
            invalid.assert();
            invalid.remove();

            let valid = server
                .mock("GET", "/index/config.json")
                .with_status(200)
                .with_body(r#"{"dl":"https://downloads.example"}"#)
                .expect(1)
                .create();

            assert_eq!(
                sparse_config(cache.path(), &client, &index).unwrap().dl,
                "https://downloads.example"
            );
            assert!(sparse_config_path(cache.path(), &index).exists());
            valid.assert();
        }
    }

    #[test]
    fn sparse_registry_returns_error_for_invalid_stripped_url() {
        let error = crate::Crate::sparse_registry("sparse+https://", "foo", "1.0.0")
            .err()
            .expect("invalid sparse URL should return an error");
        assert_eq!(
            error.downcast_ref::<url::ParseError>(),
            Some(&url::ParseError::EmptyHost)
        );
    }

    #[test]
    fn normalizes_sparse_index_urls_and_derives_config_url() {
        let index =
            normalize_sparse_index(Url::parse("sparse+https://registry.example/index").unwrap())
                .unwrap();

        assert_eq!(index.as_str(), "https://registry.example/index/");
        assert_eq!(
            index.join("config.json").unwrap().as_str(),
            "https://registry.example/index/config.json"
        );
    }

    #[test]
    fn expands_supported_download_url_markers() {
        let url = download_url(
            "https://registry.example/{crate}/{version}",
            "MyCrate",
            "1.2.3",
        )
        .unwrap();

        assert_eq!(url.as_str(), "https://registry.example/MyCrate/1.2.3");
    }

    #[test]
    fn appends_default_download_path_without_supported_markers() {
        assert_eq!(
            download_url("https://registry.example", "crate", "2.0.0")
                .unwrap()
                .as_str(),
            "https://registry.example/crate/2.0.0/download"
        );
    }

    #[test]
    fn crates_io_sparse_url_is_normalized() {
        assert_eq!(
            normalize_sparse_index(CRATES_IO_SPARSE_INDEX.clone()).unwrap(),
            *CRATES_IO_SPARSE_INDEX
        );
    }
}
