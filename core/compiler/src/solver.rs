use std::{
    collections::{HashMap, HashSet, VecDeque},
    ops::Sub,
};

use anyhow::{anyhow, Result};
use arcstr::ArcStr;
use ena::unify::{InPlaceUnificationTable, UnifyKey};
use good_lp::{default_solver, ProblemVariables, Solution, SolverModel};
use serde::{Deserialize, Serialize};

type Layer = ArcStr;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct Var(u32);

impl UnifyKey for Var {
    type Value = ();

    fn index(&self) -> u32 {
        self.0
    }
    fn from_index(u: u32) -> Self {
        Self(u)
    }
    fn tag() -> &'static str {
        "var"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attrs {
    pub source: Option<SourceInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceInfo {
    pub span: cfgrammar::Span,
    pub id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rect<T> {
    pub layer: Option<Layer>,
    pub x0: T,
    pub y0: T,
    pub x1: T,
    pub y1: T,
    pub attrs: Attrs,
}

impl<T: Sub + Clone> Rect<T> {
    fn width(&self) -> T::Output {
        self.x1.clone() - self.x0.clone()
    }
    fn height(&self) -> T::Output {
        self.y1.clone() - self.y0.clone()
    }
}

impl Rect<Var> {
    fn vars(&self) -> [Var; 4] {
        [self.x0, self.y0, self.x1, self.y1]
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConstraintAttrs {
    pub span: Option<cfgrammar::Span>,
}

#[derive(Clone, Debug)]
pub struct LinearConstraint {
    pub coeffs: Vec<(f64, Var)>,
    pub constant: f64,
    pub is_equality: bool,
    pub attrs: ConstraintAttrs,
}

#[derive(Clone, Debug)]
pub struct MaxArrayConstraint {
    // Must be fixed before the array is solved.
    pub array_cell: Rect<Var>,
    // Must be fixed before the array is solved.
    pub input_rect: Rect<Var>,
    pub x_spacing: f64,
    pub y_spacing: f64,
    /// Solved after the nx and ny of the array is solved (width/height fixed).
    pub output_rect: Rect<Var>,
    pub attrs: ConstraintAttrs,
}

#[derive(Clone, Debug)]
pub enum Constraint {
    Linear(LinearConstraint),
    MaxArray(MaxArrayConstraint),
}

#[derive(Clone, Default)]
struct Vars {
    uf: InPlaceUnificationTable<Var>,
    vars: Vec<Var>,
}

impl Vars {
    fn new_var(&mut self) -> Var {
        let var = self.uf.new_key(());
        self.vars.push(var);
        var
    }

    fn vars(&self) -> Vec<Var> {
        self.vars.clone()
    }
}

#[derive(Clone, Default)]
pub struct Cell {
    vars: Vars,
    emitted_rects: Vec<Rect<Var>>,
    constraints: Vec<Constraint>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SolvedCell {
    pub rects: Vec<Rect<f64>>,
}

impl Cell {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn var(&mut self) -> Var {
        self.vars.new_var()
    }

    pub fn rect(&mut self, attrs: Attrs) -> Rect<Var> {
        let x0 = self.var();
        let y0 = self.var();
        let x1 = self.var();
        let y1 = self.var();
        Rect {
            layer: None,
            x0,
            y0,
            x1,
            y1,
            attrs,
        }
    }

    pub fn physical_rect(&mut self, layer: Layer, attrs: Attrs) -> Rect<Var> {
        let x0 = self.var();
        let y0 = self.var();
        let x1 = self.var();
        let y1 = self.var();
        Rect {
            layer: Some(layer),
            x0,
            y0,
            x1,
            y1,
            attrs,
        }
    }

    pub fn emit_rect(&mut self, rect: Rect<Var>) {
        self.emitted_rects.push(rect);
    }

    pub fn add_constraint(&mut self, constraint: Constraint) {
        self.constraints.push(constraint);
    }

    pub fn solve(self) -> Result<SolvedCell> {
        let Cell {
            mut vars,
            emitted_rects,
            constraints,
            ..
        } = self;
        for constraint in &constraints {
            match constraint {
                Constraint::Linear(constraint) => {
                    if let Some((_, first)) = constraint.coeffs.first() {
                        for (_, var) in &constraint.coeffs {
                            vars.uf.union(*var, *first);
                        }
                    }
                }
                Constraint::MaxArray(constraint) => {
                    let mut in_vars = constraint
                        .array_cell
                        .vars()
                        .into_iter()
                        .chain(constraint.input_rect.vars());
                    let first = in_vars.next().unwrap();
                    for var in in_vars {
                        vars.uf.union(first, var);
                    }
                    let out_vars = constraint.output_rect.vars();
                    for var in out_vars {
                        vars.uf.union(out_vars[0], var);
                    }
                }
            }
        }

        // HashMap from root Var to Vec of root Var.
        let mut dag_edges = HashMap::new();
        let mut reverse_dag_edges = HashMap::new();
        for constraint in &constraints {
            match constraint {
                Constraint::Linear(_) => {}
                Constraint::MaxArray(constraint) => {
                    let start = vars.uf.find(constraint.input_rect.x0);
                    let end = vars.uf.find(constraint.output_rect.x0);
                    dag_edges.entry(start).or_insert(HashSet::new()).insert(end);
                    reverse_dag_edges
                        .entry(end)
                        .or_insert(HashSet::new())
                        .insert(start);
                }
            }
        }

        // BFS through groups.
        // Keep track of variables that have been solved.
        let mut val_map = HashMap::new();
        let mut solved_rects = Vec::new();
        let mut queue = VecDeque::new();
        for var in vars.vars() {
            if vars.uf.find(var) == var && !reverse_dag_edges.contains_key(&vars.uf.find(var)) {
                queue.push_back(var);
            }
        }
        while let Some(next) = queue.pop_front() {
            let mut lp_var_map = HashMap::new();
            let mut lp_vars = ProblemVariables::new();
            for var in vars.vars() {
                if vars.uf.find(var) == next || val_map.contains_key(&var) {
                    lp_var_map.insert(var, lp_vars.add_variable());
                }
            }
            let mut problem = lp_vars.maximise(1).using(default_solver);
            for var in vars.vars() {
                if let Some(value) = val_map.get(&var) {
                    problem.add_constraint((1 * lp_var_map[&var]).eq(*value));
                }
            }
            for constraint in &constraints {
                if let Constraint::Linear(constraint) = constraint {
                    if let Some((_, first)) = constraint.coeffs.first() {
                        if vars.uf.find(*first) == next {
                            problem.add_constraint({
                                let expr = constraint
                                    .coeffs
                                    .iter()
                                    .map(|(k, x)| *k * lp_var_map[x])
                                    .reduce(|a, b| a + b)
                                    .ok_or_else(|| {
                                        anyhow!("cannot create a constraint on constants")
                                    })?
                                    + constraint.constant;
                                if constraint.is_equality {
                                    expr.eq(0)
                                } else {
                                    expr.leq(0)
                                }
                            });
                        }
                    }
                }
            }
            let solution = problem.solve()?;
            for var in vars.vars() {
                if vars.uf.find(var) == next {
                    val_map.insert(var, solution.value(lp_var_map[&var]));
                }
            }
            for constraint in &constraints {
                if let Constraint::MaxArray(constraint) = constraint {
                    if val_map.contains_key(&constraint.input_rect.x0)
                        && !val_map.contains_key(&constraint.output_rect.x0)
                    {
                        let Rect {
                            x0,
                            y0,
                            x1,
                            y1,
                            attrs,
                            ..
                        } = &constraint.input_rect;
                        let input_rect = Rect {
                            layer: None,
                            x0: val_map[x0] as f64,
                            y0: val_map[y0] as f64,
                            x1: val_map[x1] as f64,
                            y1: val_map[y1] as f64,
                            attrs: attrs.clone(),
                        };
                        let Rect {
                            layer,
                            x0,
                            y0,
                            x1,
                            y1,
                            attrs,
                        } = &constraint.array_cell;
                        let array_cell = Rect {
                            layer: layer.clone(),
                            x0: val_map[x0] as f64,
                            y0: val_map[y0] as f64,
                            x1: val_map[x1] as f64,
                            y1: val_map[y1] as f64,
                            attrs: attrs.clone(),
                        };
                        let nx = (input_rect.x1 - input_rect.x0 + constraint.x_spacing)
                            / (array_cell.width() + constraint.x_spacing);
                        let ny = (input_rect.y1 - input_rect.y0 + constraint.y_spacing)
                            / (array_cell.height() + constraint.y_spacing);
                        let output_width =
                            array_cell.width() * nx + constraint.x_spacing * (nx - 1.);
                        let output_height =
                            array_cell.height() * ny + constraint.y_spacing * (ny - 1.);
                        let centering_offset_x =
                            (input_rect.x1 - input_rect.x0 - output_width) / 2.;
                        let centering_offset_y =
                            (input_rect.y1 - input_rect.y0 - output_height) / 2.;
                        for i in 0..nx as i64 {
                            for j in 0..ny as i64 {
                                solved_rects.push(Rect {
                                    layer: array_cell.layer.clone(),
                                    x0: centering_offset_x
                                        + input_rect.x0
                                        + (array_cell.width() + constraint.x_spacing) * i as f64,
                                    y0: centering_offset_y
                                        + input_rect.y0
                                        + (array_cell.height() + constraint.y_spacing) * j as f64,
                                    x1: array_cell.x1 + centering_offset_x + input_rect.x0
                                        - array_cell.x0
                                        + (array_cell.width() + constraint.x_spacing) * i as f64,
                                    y1: array_cell.y1 + centering_offset_y + input_rect.y0
                                        - array_cell.y0
                                        + (array_cell.height() + constraint.y_spacing) * j as f64,
                                    attrs: array_cell.attrs.clone(),
                                })
                            }
                        }
                        val_map.insert(
                            constraint.output_rect.x0,
                            input_rect.x0 + centering_offset_x,
                        );
                        val_map.insert(
                            constraint.output_rect.y0,
                            input_rect.y0 + centering_offset_y,
                        );
                        val_map.insert(
                            constraint.output_rect.x1,
                            input_rect.x0 + centering_offset_x + output_width,
                        );
                        val_map.insert(
                            constraint.output_rect.y1,
                            input_rect.y0 + centering_offset_y + output_height,
                        );
                    }
                }
            }
            if let Some(children) = dag_edges.get(&next) {
                for child in children {
                    queue.push_back(*child);
                }
            }
        }

        Ok(SolvedCell {
            rects: solved_rects
                .into_iter()
                .chain(emitted_rects.into_iter().map(
                    |Rect {
                         layer,
                         x0,
                         y0,
                         x1,
                         y1,
                         attrs,
                     }| Rect {
                        layer,
                        x0: val_map[&x0],
                        y0: val_map[&y0],
                        x1: val_map[&x1],
                        y1: val_map[&y1],
                        attrs,
                    },
                ))
                .collect(),
        })
    }
}

impl SolvedCell {
    pub fn width(&self) -> f64 {
        let mut min = f64::MAX;
        let mut max = f64::MIN;
        for rect in &self.rects {
            min = *[min, rect.x0, rect.x1]
                .iter()
                .min_by(|a, b| a.total_cmp(b))
                .unwrap();
            max = *[max, rect.x0, rect.x1]
                .iter()
                .max_by(|a, b| a.total_cmp(b))
                .unwrap();
        }
        max - min
    }

    pub fn height(&self) -> f64 {
        let mut min = f64::MAX;
        let mut max = f64::MIN;
        for rect in &self.rects {
            min = *[min, rect.y0, rect.y1]
                .iter()
                .min_by(|a, b| a.total_cmp(b))
                .unwrap();
            max = *[max, rect.y0, rect.y1]
                .iter()
                .max_by(|a, b| a.total_cmp(b))
                .unwrap();
        }
        max - min
    }

    pub fn bbox(&self) -> Rect<f64> {
        let mut min_x = f64::MAX;
        let mut max_x = f64::MIN;
        let mut min_y = f64::MAX;
        let mut max_y = f64::MIN;
        for rect in &self.rects {
            min_x = *[min_x, rect.x0, rect.x1]
                .iter()
                .min_by(|a, b| a.total_cmp(b))
                .unwrap();
            max_x = *[max_x, rect.x0, rect.x1]
                .iter()
                .max_by(|a, b| a.total_cmp(b))
                .unwrap();
            min_y = *[min_y, rect.y0, rect.y1]
                .iter()
                .min_by(|a, b| a.total_cmp(b))
                .unwrap();
            max_y = *[max_y, rect.y0, rect.y1]
                .iter()
                .max_by(|a, b| a.total_cmp(b))
                .unwrap();
        }
        Rect {
            layer: None,
            x0: min_x,
            y0: min_y,
            x1: max_x,
            y1: max_y,
            attrs: Attrs { source: None },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use gds21::{GdsBoundary, GdsElement, GdsLibrary, GdsPoint, GdsStruct};

    #[test]
    fn linear_constraints_solved_correctly() {
        let mut cell = Cell::new();
        let r1 = cell.physical_rect(arcstr::literal!("met1"), Attrs { source: None });
        let r2 = cell.physical_rect(arcstr::literal!("met1"), Attrs { source: None });
        cell.emit_rect(r1.clone());
        cell.emit_rect(r2.clone());
        cell.add_constraint(Constraint::Linear(LinearConstraint {
            coeffs: vec![(1., r1.x0)],
            constant: 0.,
            is_equality: true,
            attrs: Default::default(),
        }));
        cell.add_constraint(Constraint::Linear(LinearConstraint {
            coeffs: vec![(1., r1.y0)],
            constant: 0.,
            is_equality: true,
            attrs: Default::default(),
        }));
        cell.add_constraint(Constraint::Linear(LinearConstraint {
            coeffs: vec![(1., r1.x1), (-1., r1.x0)],
            constant: -50.,
            is_equality: true,
            attrs: Default::default(),
        }));
        cell.add_constraint(Constraint::Linear(LinearConstraint {
            coeffs: vec![(1., r2.x0), (-1., r1.x1)],
            constant: -100.,
            is_equality: true,
            attrs: Default::default(),
        }));
        cell.add_constraint(Constraint::Linear(LinearConstraint {
            coeffs: vec![(1., r2.x1), (-1., r2.x0)],
            constant: -200.,
            is_equality: true,
            attrs: Default::default(),
        }));
        cell.add_constraint(Constraint::Linear(LinearConstraint {
            coeffs: vec![(1., r1.y1), (-1., r1.y0)],
            constant: -20.,
            is_equality: true,
            attrs: Default::default(),
        }));
        cell.add_constraint(Constraint::Linear(LinearConstraint {
            coeffs: vec![(1., r2.y0), (-1., r1.y1)],
            constant: -40.,
            is_equality: true,
            attrs: Default::default(),
        }));
        cell.add_constraint(Constraint::Linear(LinearConstraint {
            coeffs: vec![(1., r2.y1), (-1., r2.y0)],
            constant: -80.,
            is_equality: true,
            attrs: Default::default(),
        }));
        let via_rect = cell.physical_rect(arcstr::literal!("via"), Attrs { source: None });
        cell.add_constraint(Constraint::Linear(LinearConstraint {
            coeffs: vec![(1., via_rect.x1), (-1., via_rect.x0)],
            constant: -5.,
            is_equality: true,
            attrs: Default::default(),
        }));
        cell.add_constraint(Constraint::Linear(LinearConstraint {
            coeffs: vec![(1., via_rect.y1), (-1., via_rect.y0)],
            constant: -5.,
            is_equality: true,
            attrs: Default::default(),
        }));
        let output_rect = cell.rect(Attrs { source: None });
        cell.add_constraint(Constraint::MaxArray(MaxArrayConstraint {
            array_cell: via_rect,
            input_rect: r2,
            x_spacing: 5.,
            y_spacing: 5.,
            output_rect: output_rect.clone(),
            attrs: Default::default(),
        }));

        let via_enclosure = cell.physical_rect(arcstr::literal!("met2"), Attrs { source: None });
        cell.add_constraint(Constraint::Linear(LinearConstraint {
            coeffs: vec![(1., via_enclosure.x1), (-1., output_rect.x1)],
            constant: -40.,
            is_equality: true,
            attrs: Default::default(),
        }));
        cell.add_constraint(Constraint::Linear(LinearConstraint {
            coeffs: vec![(-1., via_enclosure.x0), (1., output_rect.x0)],
            constant: -40.,
            is_equality: true,
            attrs: Default::default(),
        }));

        cell.add_constraint(Constraint::Linear(LinearConstraint {
            coeffs: vec![(1., via_enclosure.y1), (-1., output_rect.y1)],
            constant: -100.,
            is_equality: true,
            attrs: Default::default(),
        }));
        cell.add_constraint(Constraint::Linear(LinearConstraint {
            coeffs: vec![(-1., via_enclosure.y0), (1., output_rect.y0)],
            constant: -100.,
            is_equality: true,
            attrs: Default::default(),
        }));

        let solved_cell = cell.solve().expect("failed to solve cell");

        let mut gds = GdsLibrary::new("TOP");
        let mut cell = GdsStruct::new("cell");
        for rect in &solved_cell.rects {
            if let Some(layer) = &rect.layer {
                let layer = match layer.as_str() {
                    "met1" => 10,
                    "via" => 11,
                    "met2" => 20,
                    _ => unreachable!(),
                };
                cell.elems.push(GdsElement::GdsBoundary(GdsBoundary {
                    layer,
                    datatype: 0,
                    xy: vec![
                        GdsPoint::new(rect.x0 as i32, rect.y0 as i32),
                        GdsPoint::new(rect.x0 as i32, rect.y1 as i32),
                        GdsPoint::new(rect.x1 as i32, rect.y1 as i32),
                        GdsPoint::new(rect.x1 as i32, rect.y0 as i32),
                    ],
                    ..Default::default()
                }));
            }
        }
        gds.structs.push(cell);
        let work_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("build/linear_constraints_solved_correctly");
        std::fs::create_dir_all(&work_dir).expect("failed to create dirs");
        gds.save(work_dir.join("layout.gds"))
            .expect("failed to write GDS");
    }
}
