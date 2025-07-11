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
