use std::path::{Path, PathBuf};

/// Resolved, compiler-facing configuration for an Argon workspace.
///
/// Paths are supplied by the caller rather than discovered from a manifest so
/// the compiler remains independent of `arc`. The technology file and GDS imports
/// live here because they are workspace-wide inputs to dynamic execution.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct WorkspaceConfig {
    pub root_lib: PathBuf,
    pub dependencies: Vec<(String, PathBuf)>,
    pub tech: Option<PathBuf>,
    pub gds_imports: Vec<(String, PathBuf)>,
}

impl WorkspaceConfig {
    pub fn new(root_lib: impl Into<PathBuf>) -> Self {
        Self {
            root_lib: root_lib.into(),
            ..Self::default()
        }
    }

    pub fn root_lib(&self) -> &Path {
        &self.root_lib
    }

    pub fn with_dependencies(
        mut self,
        dependencies: impl IntoIterator<Item = (String, PathBuf)>,
    ) -> Self {
        self.dependencies = dependencies.into_iter().collect();
        self
    }

    pub fn with_gds_imports(
        mut self,
        gds_imports: impl IntoIterator<Item = (String, PathBuf)>,
    ) -> Self {
        self.gds_imports = gds_imports.into_iter().collect();
        self
    }

    pub fn with_tech(mut self, tech: impl Into<Option<PathBuf>>) -> Self {
        self.tech = tech.into();
        self
    }
}
