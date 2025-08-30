use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;

use async_compat::CompatExt;
use compiler::compile::{CompileInput, CompiledCell, Rect, compile};
use compiler::parse::parse;
use compiler::solver::Var;
use gpui::*;
use itertools::Itertools;
use lsp_server::rpc::GuiToLspClient;
use tarpc::tokio_serde::formats::Json;

use crate::rpc::SyncGuiToLspClient;

type Params = Vec<(String, f64)>;

/// Identifier for specific parametrizations of p-cell with name `name`.
struct CellId {
    name: String,
    params: Params,
}

/// Persistent state associated with a specific parametrization of a p-cell in a project.
pub struct Cell {
    pub rects: Vec<Rect<Var>>,
    pub solved_values: Vec<(Var, f64)>,
    // TODO: Use null space vectors to allow dragging coordinates.
    pub null_space: (),
    pub variable_overrides: Vec<(Var, f64)>,
}

/// Persistent state of project (i.e. anything that is saved in GUI project file).
///
/// GUI project file is saved in root directory of the associated Argon project.
pub struct Project {
    pub root: PathBuf,
    pub code: String,
    /// Specific parametrizations of p-cells that have been compiled.
    pub cells: HashMap<CellId, Cell>,
    /// Cells that are open in the GUI.
    pub open_cells: Vec<CellId>,
}

impl Project {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // TODO: Get project metadata from lsp.
        Self {
            root: PathBuf::from(""),
            code: "".to_string(),
            cells: HashMap::default(),
            open_cells: Vec::new(),
        }
    }
}
