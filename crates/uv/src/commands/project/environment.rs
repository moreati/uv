use tracing::debug;

use crate::commands::pip::loggers::{InstallLogger, ResolveLogger};
use crate::commands::project::{
    resolve_environment, sync_environment, EnvironmentSpecification, ProjectError,
};
use crate::commands::SharedState;
use crate::printer::Printer;
use crate::settings::ResolverInstallerSettings;
use uv_cache::{Cache, CacheBucket};
use uv_cache_key::{cache_digest, hash_digest};
use uv_client::Connectivity;
use uv_configuration::{Concurrency, TrustedHost};
use uv_distribution_types::{Name, Resolution};
use uv_python::{Interpreter, PythonEnvironment};

/// A [`PythonEnvironment`] stored in the cache.
#[derive(Debug)]
pub(crate) struct CachedEnvironment(PythonEnvironment);

impl From<CachedEnvironment> for PythonEnvironment {
    fn from(environment: CachedEnvironment) -> Self {
        environment.0
    }
}

impl CachedEnvironment {
    /// Get or create an [`CachedEnvironment`] based on a given set of requirements and a base
    /// interpreter.
    pub(crate) async fn get_or_create(
        spec: EnvironmentSpecification<'_>,
        interpreter: Interpreter,
        settings: &ResolverInstallerSettings,
        state: &SharedState,
        resolve: Box<dyn ResolveLogger>,
        install: Box<dyn InstallLogger>,
        connectivity: Connectivity,
        concurrency: Concurrency,
        native_tls: bool,
        allow_insecure_host: &[TrustedHost],
        cache: &Cache,
        printer: Printer,
    ) -> Result<Self, ProjectError> {
        // When caching, always use the base interpreter, rather than that of the virtual
        // environment.
        let interpreter = if let Some(interpreter) = interpreter.to_base_interpreter(cache)? {
            debug!(
                "Caching via base interpreter: `{}`",
                interpreter.sys_executable().display()
            );
            interpreter
        } else {
            debug!(
                "Caching via interpreter: `{}`",
                interpreter.sys_executable().display()
            );
            interpreter
        };

        // Resolve the requirements with the interpreter.
        let resolution = Resolution::from(
            resolve_environment(
                spec,
                &interpreter,
                settings.as_ref().into(),
                state,
                resolve,
                connectivity,
                concurrency,
                native_tls,
                allow_insecure_host,
                cache,
                printer,
            )
            .await?,
        );

        // Hash the resolution by hashing the generated lockfile.
        // TODO(charlie): If the resolution contains any mutable metadata (like a path or URL
        // dependency), skip this step.
        let resolution_hash = {
            let mut distributions = resolution.distributions().collect::<Vec<_>>();
            distributions.sort_unstable_by_key(|dist| dist.name());
            hash_digest(&distributions)
        };

        // Hash the interpreter based on its path.
        // TODO(charlie): Come up with a robust hash for the interpreter.
        let interpreter_hash = cache_digest(&interpreter.sys_executable());

        // Search in the content-addressed cache.
        let cache_entry = cache.entry(CacheBucket::Environments, interpreter_hash, resolution_hash);

        if cache.refresh().is_none() {
            if let Ok(root) = fs_err::read_link(cache_entry.path()) {
                if let Ok(environment) = PythonEnvironment::from_root(root, cache) {
                    return Ok(Self(environment));
                }
            }
        }

        // Create the environment in the cache, then relocate it to its content-addressed location.
        let temp_dir = cache.venv_dir()?;
        let venv = uv_virtualenv::create_venv(
            temp_dir.path(),
            interpreter,
            uv_virtualenv::Prompt::None,
            false,
            false,
            true,
            false,
        )?;

        sync_environment(
            venv,
            &resolution,
            settings.as_ref().into(),
            state,
            install,
            connectivity,
            concurrency,
            native_tls,
            allow_insecure_host,
            cache,
            printer,
        )
        .await?;

        // Now that the environment is complete, sync it to its content-addressed location.
        let id = cache
            .persist(temp_dir.into_path(), cache_entry.path())
            .await?;
        let root = cache.archive(&id);

        Ok(Self(PythonEnvironment::from_root(root, cache)?))
    }

    /// Convert the [`CachedEnvironment`] into an [`Interpreter`].
    pub(crate) fn into_interpreter(self) -> Interpreter {
        self.0.into_interpreter()
    }
}
