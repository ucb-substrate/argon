//! Delay-chain sizing for the replica-timed control logic.
//!
//! The enable buffers, the write-mask driver, and the row decoder are sized by
//! logical effort, and the control logic's delay chains are chosen from their
//! RC time constants.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use geometry::snap::snap_to_grid;

use crate::{ColParams, PrimitiveGateParams, SramParams};

/// Inverter counts of the control logic's delay chains.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlLogicParams {
    pub decoder_delay_invs: i32,
    pub wlen_pulse_invs: i32,
    pub pc_set_delay_invs: i32,
    pub wrdrven_set_delay_invs: i32,
}

#[derive(Clone, Copy, Debug)]
struct GateModel {
    res: f64,
    cin: f64,
    cout: f64,
}

const INV_MODEL: GateModel = GateModel {
    res: 1422.118502462849,
    cin: 0.000000000000004482092764998187,
    cout: 0.0,
};
const NAND2_MODEL: GateModel = GateModel {
    res: 1478.364147093855,
    cin: 0.000000000000005389581112035269,
    cout: 0.0,
};
const NAND3_MODEL: GateModel = GateModel {
    res: 1478.037783669641,
    cin: 0.000000000000006217130454627972,
    cout: 0.0,
};

const WORDLINE_CAP_PER_CELL: f64 = 0.000_000_000_000_014_724_682_766_764_86 / 12.0;
const WORDLINE_CAP_MAX: f64 = 500e-15;

/// Per-unit column loads, characterized on a 128-column, mux-4, 8-bit-mask array.
const PC_B_CAP: f64 = 591.432e-15 / 130.0;
const SAEN_CAP: f64 = 393.347e-15 / 32.0;
const WE_CAP: f64 = 36.462e-15 / 4.0;
const SEL_CAP: f64 = 186.458e-15 / 32.0;
const SEL_B_CAP: f64 = 198.964e-15 / 32.0;
const REF_MUX_PWIDTH: f64 = 3600.0;
const WE_I_CAP: f64 = 11.3990e-15;
const WE_IB_CAP: f64 = 12.0547e-15;
const REF_PULL_UP_WIDTH: f64 = 2000.0;
const REF_DRIVER_WIDTH: f64 = 3000.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Primitive {
    Inv,
    Nand2,
    Nand3,
}

impl Primitive {
    fn model(self) -> GateModel {
        match self {
            Primitive::Inv => INV_MODEL,
            Primitive::Nand2 => NAND2_MODEL,
            Primitive::Nand3 => NAND3_MODEL,
        }
    }

    /// NMOS width of the 1x gate.
    fn unit_nwidth(self) -> i64 {
        match self {
            Primitive::Inv => 1_000,
            Primitive::Nand2 => 2_000,
            Primitive::Nand3 => 3_000,
        }
    }

    /// PMOS width of the 1x gate.
    fn unit_pwidth(self) -> i64 {
        2_500
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Sized {
    gate: Primitive,
    nwidth: i64,
    pwidth: i64,
}

impl Sized {
    fn unit(gate: Primitive) -> Self {
        Self {
            gate,
            nwidth: gate.unit_nwidth(),
            pwidth: gate.unit_pwidth(),
        }
    }

    /// Scales a 1x gate, snapping widths to 50 nm.
    fn scaled(gate: Primitive, factor: f64) -> Self {
        Self {
            gate,
            nwidth: snap_to_grid((gate.unit_nwidth() as f64 * factor).round() as i64, 50),
            pwidth: snap_to_grid((gate.unit_pwidth() as f64 * factor).round() as i64, 50),
        }
    }

    fn scale(self) -> f64 {
        self.nwidth as f64 / self.gate.unit_nwidth() as f64
    }

    fn cin(self) -> f64 {
        self.scale() * self.gate.model().cin
    }
}

/// Elmore time constant of a gate chain driving `cl`.
fn chain_time_constant(gates: &[Sized], cl: f64) -> f64 {
    gates
        .iter()
        .enumerate()
        .map(|(i, gate)| {
            let model = gate.gate.model();
            let scale = gate.scale();
            let cin_next = gates.get(i + 1).map_or(cl, |next| next.cin());
            model.res / scale * (model.cout * scale + cin_next)
        })
        .sum()
}

/// Learning-rate schedule for sizing a gate chain by gradient descent.
#[derive(Clone, Copy)]
struct OptimizerOpts {
    lr: f64,
    lr_decay: f64,
    max_iter: usize,
}

const DECODER_OPTS: OptimizerOpts = OptimizerOpts {
    lr: 1e10,
    lr_decay: 0.999995,
    max_iter: 10_000_000,
};

const BUFFER_OPTS: OptimizerOpts = OptimizerOpts {
    lr: 1e11,
    lr_decay: 0.999999,
    max_iter: 10_000_000,
};

/// A gate chain whose first gate is 1x and whose later gates have one size
/// variable each. The input node of gate `k` also carries `branch[k]` times
/// that gate's size, modeling sibling gates. Floating-point operations run in
/// a fixed order so that sizing results are reproducible bit for bit.
struct LogicPath {
    models: Vec<GateModel>,
    branch: Vec<Option<f64>>,
    load: f64,
    values: Vec<f64>,
}

impl LogicPath {
    fn value(&self, var: usize) -> f64 {
        f64::max(self.values[var], 1.0)
    }

    /// Delay of segment `k`, which drives the input of segment `k + 1`.
    /// Adds the delay's gradient to `grad` when given.
    fn segment_delay(&self, k: usize, grad: Option<&mut [f64]>) -> f64 {
        let model = self.models[k];
        let (r, mut c, own) = if k == 0 {
            (model.res, model.cout, None)
        } else {
            let x = self.value(k - 1);
            (
                model.res / x,
                model.cout * x,
                Some((k - 1, -model.res / (x * x), 0.0 + model.cout)),
            )
        };
        c += if k + 1 == self.models.len() {
            self.load
        } else {
            0.0
        };
        let mut next: Option<(usize, f64)> = None;
        if let Some(mult) = self.branch.get(k + 1).copied().flatten() {
            c += self.value(k) * mult;
            next = Some((k, 0.0 + mult));
        }
        if let Some(next_model) = self.models.get(k + 1) {
            c += 0.0 + next_model.cin * self.value(k);
            next = Some((k, next.map_or(0.0, |(_, d)| d) + next_model.cin));
        }
        if let Some(grad) = grad {
            if let Some((v, drdv, dcdv)) = own {
                grad[v] += r * dcdv + c * drdv;
            }
            if let Some((v, dcdv)) = next {
                grad[v] += r * dcdv + c * 0.0;
            }
        }
        r * c
    }

    fn delay(&self) -> f64 {
        let mut tau = 0.0;
        for k in 0..self.models.len() {
            tau += self.segment_delay(k, None);
        }
        tau
    }

    fn delay_grad(&self, grad: &mut [f64]) -> f64 {
        let mut tau = 0.0;
        for k in 0..self.models.len() {
            tau += self.segment_delay(k, Some(grad));
        }
        tau
    }

    /// Gradient descent with backtracking: a step that increases the delay is
    /// undone and retried at half the size. With `early_exit`, it stops at the
    /// first accepted step that leaves every value unchanged, which gives the
    /// same result as running all `max_iter` steps.
    fn size(&mut self, opts: OptimizerOpts, early_exit: bool) {
        let n = self.values.len();
        let mut lr = opts.lr;
        let mut base = vec![0.0; n];
        let mut base_grad = vec![0.0; n];
        let mut grad = vec![0.0; n];
        let mut base_delay = f64::INFINITY;
        let mut step = lr;
        let mut iter = 0;
        while iter < opts.max_iter {
            grad.fill(0.0);
            let delay = self.delay_grad(&mut grad);
            let accepted = delay <= base_delay * (1.0 + 1e-9);
            if accepted {
                base_delay = delay;
                base.copy_from_slice(&self.values);
                base_grad.copy_from_slice(&grad);
                step = lr;
                lr *= opts.lr_decay;
                iter += 1;
            } else {
                step *= 0.5;
                if step == 0.0 {
                    self.values.copy_from_slice(&base);
                    return;
                }
            }
            let mut moved = false;
            for i in 0..n {
                let next = f64::max(base[i] - step * base_grad[i], 1.0);
                moved |= next.to_bits() != base[i].to_bits();
                self.values[i] = next;
            }
            // Every later step starts from these same values, so it has the
            // same gradient and is accepted, and its step size is no larger, so
            // rounding returns each value to the same bits again.
            if early_exit && accepted && !moved {
                break;
            }
        }
        if self.delay() > base_delay * (1.0 + 1e-9) {
            self.values.copy_from_slice(&base);
        }
    }
}

/// Sizes a chain driving `load`, returning each gate's size from input to
/// output; the first gate is 1x. See [`LogicPath`].
fn size_chain(models: &[GateModel], branch: &[f64], load: f64, opts: OptimizerOpts) -> Vec<f64> {
    // Each problem takes tens of millions of descent steps; sizing many
    // organizations in one process repeats many of them.
    static SOLVED: OnceLock<Mutex<HashMap<Vec<u64>, Vec<f64>>>> = OnceLock::new();
    let key = chain_key(models, branch, load, opts);
    #[cfg(test)]
    tests::ASKED
        .with_borrow_mut(|asked| asked.push((models.to_vec(), branch.to_vec(), load, opts)));
    let solved = SOLVED.get_or_init(Default::default);
    if let Some(sizes) = solved.lock().unwrap().get(&key) {
        return sizes.clone();
    }
    let sizes = solve_chain(models, branch, load, opts, true);
    solved.lock().unwrap().insert(key, sizes.clone());
    sizes
}

/// Identifies a sizing problem by the bits of everything that determines its
/// solution.
fn chain_key(models: &[GateModel], branch: &[f64], load: f64, opts: OptimizerOpts) -> Vec<u64> {
    models
        .iter()
        .flat_map(|m| [m.res, m.cin, m.cout])
        .chain(branch.iter().copied())
        .chain([load, opts.lr, opts.lr_decay, opts.max_iter as f64])
        .map(f64::to_bits)
        .collect()
}

fn solve_chain(
    models: &[GateModel],
    branch: &[f64],
    load: f64,
    opts: OptimizerOpts,
    early_exit: bool,
) -> Vec<f64> {
    let mut path = LogicPath {
        models: models.to_vec(),
        branch: branch.iter().map(|&b| (b > 0.0).then_some(b)).collect(),
        load,
        values: vec![2.0; models.len() - 1],
    };
    path.size(opts, early_exit);
    std::iter::once(1.0)
        .chain((0..path.values.len()).map(|v| path.value(v)))
        .collect()
}

/// Gates of one decoder tree node, listed from input to output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NodeGate {
    And2,
    And3,
    Inv,
}

impl NodeGate {
    fn primitives(self) -> &'static [Primitive] {
        match self {
            NodeGate::And2 => &[Primitive::Nand2, Primitive::Inv],
            NodeGate::And3 => &[Primitive::Nand3, Primitive::Inv],
            NodeGate::Inv => &[Primitive::Inv],
        }
    }

    fn logical_effort(self) -> f64 {
        match self {
            NodeGate::And2 => 4. / 3.,
            NodeGate::And3 => 5. / 3.,
            NodeGate::Inv => 1.,
        }
    }
}

/// An unsized decoder tree: a gate with `num` one-hot outputs.
#[derive(Clone, Debug)]
struct PlanNode {
    gate: NodeGate,
    num: usize,
    children: Vec<PlanNode>,
}

/// A sized decoder tree node; `gates` runs from input to output.
#[derive(Clone, Debug)]
struct TreeNode {
    gates: Vec<Sized>,
    num: usize,
    children: Vec<TreeNode>,
    child_nums: Vec<usize>,
}

fn partition_bits(bits: usize, top: bool) -> Vec<usize> {
    if top {
        let right = bits / 2;
        return vec![bits - right, right];
    }
    if bits.is_multiple_of(2) {
        vec![bits / 2, bits / 2]
    } else if bits / 3 >= 2 {
        match bits % 3 {
            0 => vec![bits / 3, bits / 3, bits / 3],
            1 => vec![bits / 3 + 1, bits / 3, bits / 3],
            _ => vec![bits / 3 + 1, bits / 3 + 1, bits / 3],
        }
    } else {
        let right = bits / 2;
        vec![bits - right, right]
    }
}

fn plan_decoder(bits: usize, top: bool) -> PlanNode {
    assert!(bits > 1, "a decoder needs at least two address bits");
    match bits {
        2 => PlanNode {
            gate: NodeGate::And2,
            num: 4,
            children: vec![],
        },
        3 => PlanNode {
            gate: NodeGate::And3,
            num: 8,
            children: vec![],
        },
        _ => {
            let split = partition_bits(bits, top);
            PlanNode {
                gate: if split.len() == 2 {
                    NodeGate::And2
                } else {
                    NodeGate::And3
                },
                num: 1 << bits,
                children: split.into_iter().map(|b| plan_decoder(b, false)).collect(),
            }
        }
    }
}

impl PlanNode {
    fn with_invs(self, invs: usize) -> Self {
        if invs == 0 {
            self
        } else {
            PlanNode {
                gate: NodeGate::Inv,
                num: self.num,
                children: vec![self.with_invs(invs - 1)],
            }
        }
    }

    fn min_depth(&self) -> usize {
        self.gate.primitives().len()
            + self
                .children
                .iter()
                .map(|c| c.min_depth())
                .min()
                .unwrap_or(0)
    }

    /// Worst-case product of logical effort and branching over all paths.
    fn le_b(&self) -> f64 {
        self.gate.logical_effort()
            * self
                .children
                .iter()
                .map(|c| c.le_b() * (self.num / c.num) as f64)
                .reduce(f64::max)
                .unwrap_or(1.)
    }

    fn left_path(&self) -> Vec<&PlanNode> {
        let mut path = vec![self];
        while let Some(child) = path.last().unwrap().children.first() {
            path.push(child);
        }
        path
    }
}

impl TreeNode {
    /// Input capacitance that child `idx` sees as its load.
    fn value_for_child(&self, idx: usize) -> f64 {
        let first = self.gates[0];
        (self.num / self.child_nums[idx]) as f64 * first.gate.model().cin * first.nwidth as f64
            / first.gate.unit_nwidth() as f64
    }

    fn time_constant(&self, cl: f64) -> f64 {
        chain_time_constant(&self.gates, cl)
            + self
                .children
                .iter()
                .enumerate()
                .map(|(i, child)| child.time_constant(self.value_for_child(i)))
                .reduce(f64::max)
                .unwrap_or(0.0)
    }
}

/// Sizes the leftmost root-to-leaf path of `plan` for `load`, then recursively
/// sizes every other subtree for the input load its parent presents.
fn size_tree(plan: &PlanNode, load: f64) -> TreeNode {
    let path = plan.left_path();
    let mut models = Vec::new();
    let mut branch = Vec::new();
    for node in path.iter().rev() {
        for (j, gate) in node.gate.primitives().iter().enumerate() {
            let model = gate.model();
            let siblings = if j == 0 && !node.children.is_empty() {
                (node.num / node.children[0].num - 1) as f64
            } else {
                0.0
            };
            models.push(model);
            branch.push(siblings * model.cin);
        }
    }
    let x = size_chain(&models, &branch, load, DECODER_OPTS);
    // Path nodes run from output to input, so consume sizes in that order.
    let mut sizes = x.into_iter().rev().map(|v| v.max(0.8));
    let mut nodes = Vec::new();
    for node in &path {
        let mut gates = node
            .gate
            .primitives()
            .iter()
            .rev()
            .map(|&gate| Sized::scaled(gate, sizes.next().unwrap()))
            .collect::<Vec<_>>();
        gates.reverse();
        nodes.push(TreeNode {
            gates,
            num: node.num,
            children: vec![],
            child_nums: node.children.iter().map(|c| c.num).collect(),
        });
    }
    // Attach the other subtrees of each path node, deepest first.
    let mut child: Option<TreeNode> = None;
    for (node, plan_node) in nodes.into_iter().zip(&path).rev() {
        let mut node = node;
        node.children.extend(child.take());
        for i in 1..plan_node.children.len() {
            let sub = size_tree(&plan_node.children[i], node.value_for_child(i));
            node.children.push(sub);
        }
        child = Some(node);
    }
    child.unwrap()
}

/// The decoder tree for `bits` address bits driving `cload` per output.
fn decoder_tree(bits: usize, cload: f64) -> TreeNode {
    let plan = plan_decoder(bits, true);
    let stages = (cload / INV_MODEL.cin * plan.le_b()).log(3.).ceil() as usize;
    let depth = plan.min_depth();
    let plan = if stages > depth {
        let invs = usize::max(1, (stages + 1 - depth) / 2) * 2;
        plan.with_invs(invs)
    } else {
        plan
    };
    size_tree(&plan, cload)
}

/// Sizes a chain whose first gate is 1x `first`, followed by `invs` inverters,
/// driving `cl`. Returns the gates from input to output.
fn size_gate_chain(first: Primitive, invs: usize, cl: f64) -> Vec<Sized> {
    let models = std::iter::once(first.model())
        .chain(std::iter::repeat_n(INV_MODEL, invs))
        .collect::<Vec<_>>();
    let x = size_chain(&models, &vec![0.0; models.len()], cl, BUFFER_OPTS);
    std::iter::once(Sized::unit(first))
        .chain(
            x[1..]
                .iter()
                .map(|&v| Sized::scaled(Primitive::Inv, v.max(0.5))),
        )
        .collect()
}

fn buffer_chain_num_stages(cl: f64) -> usize {
    let fo = cl / INV_MODEL.cin;
    if fo < 4.0 {
        return 2;
    }
    let stages = 2 * (fo.log(3.0) / 2.0).round() as usize;
    if stages == 0 { 2 } else { stages }
}

/// An even-length inverter chain buffering a 1x input onto `cl`.
fn fanout_buffer(cl: f64) -> Vec<Sized> {
    size_gate_chain(Primitive::Inv, buffer_chain_num_stages(cl) - 1, cl)
}

/// The driver of one write-mask bit, which drives the write drivers of its
/// group through a NAND2 and a buffer whose last inverter is doubled. Returns
/// the gates from input to output and the load they drive.
fn wmask_driver(granularity: i32, driver_width: i32) -> (Vec<Sized>, f64) {
    let driver_scale = driver_width as f64 / REF_DRIVER_WIDTH;
    let granularity = granularity as f64;
    let cl_max = f64::max(
        granularity * WE_I_CAP * driver_scale,
        granularity * WE_IB_CAP * driver_scale,
    );
    let mut gates = size_gate_chain(Primitive::Nand2, buffer_chain_num_stages(cl_max), cl_max);
    gates.push(*gates.last().unwrap());
    (gates, cl_max)
}

/// NMOS and PMOS widths of the write-mask driver's gates, from input to output.
pub(crate) fn wmask_driver_widths(granularity: i32, driver_width: i32) -> Vec<(i32, i32)> {
    wmask_driver(granularity, driver_width)
        .0
        .iter()
        .map(|g| (g.nwidth as i32, g.pwidth as i32))
        .collect()
}

/// Sizes the control logic's delay chains for an SRAM with columns `col`.
pub fn control_logic_params(p: SramParams, col: &ColParams) -> ControlLogicParams {
    let rows = (p.num_words / p.mux_ratio) as usize;
    let row_bits = rows.ilog2() as usize;
    let addr_width = p.num_words.ilog2() as usize;
    let word_length = (col.cols / col.mux.mux_ratio) as usize;
    let wmask_bits = word_length / col.wmask_granularity as usize;
    let inv_tau = INV_MODEL.res * (INV_MODEL.cin + INV_MODEL.cout);

    let wl_cap = (col.cols + 4) as f64 * WORDLINE_CAP_PER_CELL * 1.5;
    let clamped_wl_cap = f64::min(wl_cap, WORDLINE_CAP_MAX);
    let pc_b_cap =
        PC_B_CAP * (col.cols + 4) as f64 * col.pc.pull_up_width as f64 / REF_PULL_UP_WIDTH;
    let wlen_cap = NAND2_MODEL.cin * (addr_width * 2) as f64;
    let wrdrven_cap = WE_CAP * wmask_bits as f64;
    let saen_cap = SAEN_CAP * word_length as f64;

    // The address gate is a 1x NAND2 driving a 4x inverter that drives four NAND3s.
    let addr_gate = [
        Sized::unit(Primitive::Nand2),
        Sized {
            gate: Primitive::Inv,
            nwidth: 4_000,
            pwidth: 10_000,
        },
    ];
    let addr_gate_tau = chain_time_constant(&addr_gate, NAND3_MODEL.cin * 4.);
    let wlen_tau = chain_time_constant(&fanout_buffer(wlen_cap), wlen_cap);
    let pcb_tau = chain_time_constant(&fanout_buffer(pc_b_cap), pc_b_cap);
    let sae_tau = chain_time_constant(&fanout_buffer(saen_cap), saen_cap);

    let (wmask_buffer, cl_max) = wmask_driver(col.wmask_granularity, col.wrdriver.pwidth_driver);
    let wrdrven_tau = chain_time_constant(&fanout_buffer(wrdrven_cap), wrdrven_cap)
        + chain_time_constant(&wmask_buffer, cl_max);

    let pc_set_delay_invs = ((1.2 * (1.35 * f64::max(wrdrven_tau, sae_tau) - pcb_tau) / inv_tau)
        / 2.0)
        .max(0.)
        .ceil() as i32
        * 2
        + 8;
    let wrdrven_set_delay_invs = (((1.1 * pcb_tau - wrdrven_tau) / inv_tau) / 2.0)
        .max(1.)
        .round() as i32
        * 2;

    let row_decoder = decoder_tree(row_bits, clamped_wl_cap);
    let decoder_tau = row_decoder.time_constant(wl_cap);
    let decoder_delay_invs = (f64::max(
        4.0,
        (decoder_tau + addr_gate_tau + wlen_tau - f64::min(sae_tau, wrdrven_tau)) / inv_tau,
    ) / 2.0)
        .round() as i32
        * 2
        + 2;
    let wlen_pulse_invs = (f64::max(
        2.0,
        (0.25 * decoder_tau + 6.0 * (decoder_tau - row_decoder.time_constant(clamped_wl_cap)))
            / inv_tau,
    ) / 2.0)
        .round() as i32
        * 2
        + 9;

    ControlLogicParams {
        decoder_delay_invs,
        wlen_pulse_invs,
        pc_set_delay_invs,
        wrdrven_set_delay_invs,
    }
}

/// Gate sizes of one decoder stage, from input to output: a NAND2 and its
/// inverter, then the buffer inverters after them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AndStageSizing {
    /// 2 for an AND2 stage, 3 for an AND3 stage.
    pub inputs: i32,
    pub nand: PrimitiveGateParams,
    pub inv: PrimitiveGateParams,
    pub invs: Vec<PrimitiveGateParams>,
}

/// Gate sizes of a buffer chain, from input to output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferSizing {
    pub stages: Vec<PrimitiveGateParams>,
}

/// One row decoder node: an AND2 or AND3 of a NAND and its inverter, then any
/// buffer inverters, with `num` one-hot outputs. Its inputs come from the
/// nodes listed in `children`, or from address bits when it has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecoderNodeSizing {
    pub inputs: i32,
    pub nand: PrimitiveGateParams,
    pub inv: PrimitiveGateParams,
    pub invs: Vec<PrimitiveGateParams>,
    pub num: i32,
    pub children: Vec<i32>,
}

/// The row decoder tree as a node list, with the root first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowDecoderSizing {
    pub nodes: Vec<DecoderNodeSizing>,
}

/// Sizes of the decoders and enable buffers around the array.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeripherySizing {
    pub row_decoder: RowDecoderSizing,
    pub col_decoder: AndStageSizing,
    pub pc_b_buffer: BufferSizing,
    pub sense_en_buffer: BufferSizing,
    pub write_driver_en_buffer: BufferSizing,
    pub wlen_buffer: BufferSizing,
}

fn gate_params(g: Sized) -> PrimitiveGateParams {
    PrimitiveGateParams {
        nwidth: g.nwidth as i32,
        pwidth: g.pwidth as i32,
        length: 150,
    }
}

/// Flattens a chain of inverter nodes ending in an AND2 or AND3 node, or
/// returns `None` for any other stage shape.
fn and_stage(node: &TreeNode) -> Option<(AndStageSizing, &TreeNode)> {
    let mut invs = Vec::new();
    let mut node = node;
    while node.gates.len() == 1 && node.gates[0].gate == Primitive::Inv {
        invs.push(gate_params(node.gates[0]));
        node = &node.children[0];
    }
    invs.reverse();
    let inputs = match node.gates[0].gate {
        Primitive::Nand2 => 2,
        Primitive::Nand3 => 3,
        _ => return None,
    };
    if node.gates.len() != 2 {
        return None;
    }
    let stage = AndStageSizing {
        inputs,
        nand: gate_params(node.gates[0]),
        inv: gate_params(node.gates[1]),
        invs,
    };
    Some((stage, node))
}

/// Appends `node` and its subtree to `nodes` in preorder and returns its index.
fn push_decoder_node(node: &TreeNode, nodes: &mut Vec<DecoderNodeSizing>) -> usize {
    let mut invs = Vec::new();
    let mut node = node;
    while node.gates.len() == 1 && node.gates[0].gate == Primitive::Inv {
        invs.push(gate_params(node.gates[0]));
        node = &node.children[0];
    }
    invs.reverse();
    let inputs = match node.gates[0].gate {
        Primitive::Nand2 => 2,
        Primitive::Nand3 => 3,
        Primitive::Inv => unreachable!("decoder nodes start with a NAND"),
    };
    let index = nodes.len();
    nodes.push(DecoderNodeSizing {
        inputs,
        nand: gate_params(node.gates[0]),
        inv: gate_params(node.gates[1]),
        invs,
        num: node.num as i32,
        children: vec![],
    });
    let children = node
        .children
        .iter()
        .map(|child| push_decoder_node(child, nodes) as i32)
        .collect();
    nodes[index].children = children;
    index
}

fn buffer(cl: f64) -> BufferSizing {
    BufferSizing {
        stages: fanout_buffer(cl).into_iter().map(gate_params).collect(),
    }
}

/// Sizes the row and column decoders and the enable buffers. Returns `None`
/// unless the column decoder is a single AND2 or AND3 stage, the shapes the
/// Argon layout supports.
pub fn periphery_sizing(p: SramParams, col: &ColParams) -> Option<PeripherySizing> {
    let rows = (p.num_words / p.mux_ratio) as usize;
    let row_bits = rows.ilog2() as usize;
    let col_bits = (p.mux_ratio as usize).ilog2() as usize;
    let addr_width = p.num_words.ilog2() as usize;
    let word_length = (col.cols / col.mux.mux_ratio) as usize;
    let wmask_bits = word_length / col.wmask_granularity as usize;

    let wl_cap = (col.cols + 4) as f64 * WORDLINE_CAP_PER_CELL * 1.5;
    let row_tree = decoder_tree(row_bits, f64::min(wl_cap, WORDLINE_CAP_MAX));
    let mut nodes = Vec::new();
    push_decoder_node(&row_tree, &mut nodes);

    let sel_scale = word_length as f64 * col.mux.pwidth as f64 / REF_MUX_PWIDTH;
    let col_tree = decoder_tree(col_bits, (SEL_CAP + SEL_B_CAP) * sel_scale);
    let (col_decoder, col_node) = and_stage(&col_tree)?;
    if !col_node.children.is_empty() {
        return None;
    }

    let pc_b_cap =
        PC_B_CAP * (col.cols + 4) as f64 * col.pc.pull_up_width as f64 / REF_PULL_UP_WIDTH;
    Some(PeripherySizing {
        row_decoder: RowDecoderSizing { nodes },
        col_decoder,
        pc_b_buffer: buffer(pc_b_cap),
        sense_en_buffer: buffer(SAEN_CAP * word_length as f64),
        write_driver_en_buffer: buffer(WE_CAP * wmask_bits as f64),
        wlen_buffer: buffer(NAND2_MODEL.cin * (addr_width * 2) as f64),
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashSet;

    use super::*;

    type Problem = (Vec<GateModel>, Vec<f64>, f64, OptimizerOpts);

    thread_local! {
        /// The chains `size_chain` is asked to size on this thread.
        pub(super) static ASKED: RefCell<Vec<Problem>> = const { RefCell::new(Vec::new()) };
    }

    #[test]
    fn early_exit_matches_the_full_descent() {
        for (granularity, mux_ratio, num_words, data_width) in [(8, 4, 64, 24), (8, 8, 2048, 64)] {
            SramParams::new(granularity, mux_ratio, num_words, data_width)
                .unwrap()
                .size();
        }
        let bits = |sizes: Vec<f64>| sizes.into_iter().map(f64::to_bits).collect::<Vec<_>>();
        let mut seen = HashSet::new();
        for (models, branch, load, opts) in ASKED.take() {
            if !seen.insert(chain_key(&models, &branch, load, opts)) {
                continue;
            }
            assert_eq!(
                bits(solve_chain(&models, &branch, load, opts, true)),
                bits(solve_chain(&models, &branch, load, opts, false)),
                "{models:?} {branch:?} {load}"
            );
        }
        assert!(seen.len() >= 5, "only {} chains sized", seen.len());
    }
}
