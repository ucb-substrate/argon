//! Rust-side electrical and architectural sizing for the native SRAM example.
//!
//! Argon should only construct geometry.  This crate mirrors the sizing policy
//! used by the SRAM22 port and renders the resulting scalar parameters as an
//! Argon record.  The checked-in 64-word by 24-bit preset is generated from
//! this code.

use std::fmt::Write as _;

use geometry::snap::snap_to_grid;

const GRID: i32 = 50;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SramParams {
    pub wmask_granularity: i32,
    pub mux_ratio: i32,
    pub num_words: i32,
    pub data_width: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrimitiveGateParams {
    pub nwidth: i32,
    pub pwidth: i32,
    pub length: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiffLatchParams {
    pub inv_in: PrimitiveGateParams,
    pub lch: i32,
    pub inv_out: PrimitiveGateParams,
    pub invq: PrimitiveGateParams,
    pub nwidth: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrechargeParams {
    pub length: i32,
    pub pull_up_width: i32,
    pub equalizer_width: i32,
    pub en_b_width: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TGateMuxParams {
    pub length: i32,
    pub pwidth: i32,
    pub nwidth: i32,
    pub mux_ratio: i32,
    pub idx: i32,
    pub sel_width: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriteDriverParams {
    pub length: i32,
    pub pwidth_driver: i32,
    pub nwidth_driver: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecoderStageParams {
    pub nwidth: i32,
    pub pwidth: i32,
    pub folds: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecoderParams {
    pub stages: Vec<DecoderStageParams>,
    pub max_folds: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColParams {
    pub pc: PrechargeParams,
    pub mux: TGateMuxParams,
    pub wrdriver: WriteDriverParams,
    pub latch: DiffLatchParams,
    pub cols: i32,
    pub include_wmask: bool,
    pub wmask_granularity: i32,
    pub wmask_decoder: DecoderParams,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplicaColumnParams {
    pub gate_target: i32,
    pub drain_n_target: i32,
    pub drain_p_target: i32,
    pub max_height: i32,
    pub gate_width: i32,
    pub drain_n_width: i32,
    pub drain_p_width: i32,
    pub units: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplicaRoutingParams {
    pub m0_width: i32,
    pub m0_height: i32,
    pub m1_width: i32,
    pub m1_height: i32,
    pub tracks: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SramLayoutParams {
    pub sram: SramParams,
    pub col: ColParams,
    pub standalone_col: ColParams,
    pub replica_rows: i32,
    pub address_dff_count: i32,
    pub replica_precharge: PrechargeParams,
    pub replica_column: ReplicaColumnParams,
    pub replica_routing: ReplicaRoutingParams,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SizingError {
    NonPositive(&'static str),
    UnsupportedMuxRatio(i32),
    NotDivisible(&'static str),
    NotPowerOfTwo(i32),
}

impl std::fmt::Display for SizingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonPositive(name) => write!(f, "{name} must be positive"),
            Self::UnsupportedMuxRatio(value) => {
                write!(f, "mux ratio {value} is unsupported; use 4 or 8")
            }
            Self::NotDivisible(what) => write!(f, "{what} must divide evenly"),
            Self::NotPowerOfTwo(value) => write!(f, "word count {value} must be a power of two"),
        }
    }
}

impl std::error::Error for SizingError {}

impl SramParams {
    pub fn new(
        wmask_granularity: i32,
        mux_ratio: i32,
        num_words: i32,
        data_width: i32,
    ) -> Result<Self, SizingError> {
        for (name, value) in [
            ("write-mask granularity", wmask_granularity),
            ("mux ratio", mux_ratio),
            ("word count", num_words),
            ("data width", data_width),
        ] {
            if value <= 0 {
                return Err(SizingError::NonPositive(name));
            }
        }
        if !matches!(mux_ratio, 4 | 8) {
            return Err(SizingError::UnsupportedMuxRatio(mux_ratio));
        }
        if num_words % mux_ratio != 0 {
            return Err(SizingError::NotDivisible("word count by mux ratio"));
        }
        if data_width % wmask_granularity != 0 {
            return Err(SizingError::NotDivisible(
                "data width by write-mask granularity",
            ));
        }
        if !(num_words as u32).is_power_of_two() {
            return Err(SizingError::NotPowerOfTwo(num_words));
        }
        Ok(Self {
            wmask_granularity,
            mux_ratio,
            num_words,
            data_width,
        })
    }

    pub fn size(self) -> SramLayoutParams {
        let standalone_col = practical_col_params(self);
        let mut col = standalone_col.clone();
        col.pc.en_b_width = 320 + 680 * (pc_b_routing_tracks(&col) - 1);
        col.mux.sel_width = 320 + 680 * (col_decoder_routing_tracks(&col) - 1);

        let replica_precharge = PrechargeParams {
            length: standalone_col.pc.length,
            pull_up_width: scale_width(standalone_col.pc.pull_up_width, 1.0 / 6.0, 800),
            equalizer_width: scale_width(standalone_col.pc.equalizer_width, 1.0 / 6.0, 500),
            en_b_width: 360,
        };
        let replica_column = replica_column_params(self, &standalone_col, replica_precharge);
        let replica_routing = replica_routing_params(self, &standalone_col, &col, replica_column);

        SramLayoutParams {
            sram: self,
            col,
            standalone_col,
            replica_rows: ((self.num_words / self.mux_ratio + 25) / 24) * 4,
            address_dff_count: self.num_words.ilog2() as i32 + 2,
            replica_precharge,
            replica_column,
            replica_routing,
        }
    }
}

fn round_positive(value: f64) -> i32 {
    (value + 0.5) as i32
}

fn snap_50(value: i32) -> i32 {
    snap_to_grid(i64::from(value), i64::from(GRID)) as i32
}

fn scale_width(width: i32, scale: f64, minimum: i32) -> i32 {
    snap_50(round_positive(width as f64 * scale).max(minimum))
}

fn ceil_div(a: i32, b: i32) -> i32 {
    (a + b - 1) / b
}

fn clamp(value: i32, minimum: i32, maximum: i32) -> i32 {
    value.clamp(minimum, maximum)
}

fn default_latch_params() -> DiffLatchParams {
    let inv = PrimitiveGateParams {
        nwidth: 1200,
        pwidth: 2000,
        length: 150,
    };
    DiffLatchParams {
        inv_in: inv,
        lch: 150,
        inv_out: inv,
        invq: inv,
        nwidth: 2000,
    }
}

fn practical_col_params(p: SramParams) -> ColParams {
    const BITLINE_CAP_PER_CELL: f64 = 0.000_000_000_000_088_593_641_779_370_68 / 128.0;
    const PC_B_CAP: f64 = 0.000_000_000_000_591_432 / 130.0;
    const SEL_CAP: f64 = 0.000_000_000_000_186_458 / 32.0;
    const WE_CAP: f64 = 0.000_000_000_000_036_462 / 4.0;

    let rows = p.num_words / p.mux_ratio;
    let bitline_cap = (rows + 4) as f64 * BITLINE_CAP_PER_CELL;
    let pc_scale = bitline_cap / PC_B_CAP / 8.0;
    let mux_scale = bitline_cap / SEL_CAP / 8.0;
    let wrdriver_scale = bitline_cap / WE_CAP / 6.0;
    let wrdriver = WriteDriverParams {
        length: 150,
        pwidth_driver: scale_width(3000, wrdriver_scale, 1200),
        nwidth_driver: scale_width(3000, wrdriver_scale, 1200),
    };

    ColParams {
        pc: PrechargeParams {
            length: 150,
            pull_up_width: scale_width(2000, pc_scale, 800),
            equalizer_width: scale_width(1200, pc_scale, 500),
            en_b_width: 360,
        },
        mux: TGateMuxParams {
            length: 150,
            pwidth: scale_width(3600, mux_scale, 1800),
            nwidth: scale_width(2400, mux_scale, 1200),
            mux_ratio: p.mux_ratio,
            idx: p.mux_ratio / 2,
            sel_width: 360,
        },
        wrdriver,
        latch: default_latch_params(),
        cols: p.data_width * p.mux_ratio,
        include_wmask: true,
        wmask_granularity: p.wmask_granularity,
        wmask_decoder: decoder_params(p.wmask_granularity, p.mux_ratio, wrdriver.pwidth_driver),
    }
}

fn pc_b_routing_tracks(col: &ColParams) -> i32 {
    clamp(
        ceil_div((col.cols + 4) * col.pc.pull_up_width, 256 * 2000),
        2,
        8,
    )
}

fn col_decoder_routing_tracks(col: &ColParams) -> i32 {
    let outputs = col.cols / col.mux.mux_ratio;
    clamp(ceil_div(outputs * col.mux.pwidth, 64 * 3600), 2, 4)
}

fn powi(value: f64, exponent: i32) -> f64 {
    (0..exponent).fold(1.0, |product, _| product * value)
}

fn nth_root(value: f64, degree: i32) -> f64 {
    // Match the old Argon implementation exactly: Newton iteration starts at
    // 3 and runs 24 times.  Avoiding `powf` also avoids platform-dependent
    // rounding near the 50 nm sizing grid.
    let mut estimate = 3.0;
    for _ in 0..24 {
        estimate =
            ((degree - 1) as f64 * estimate + value / powi(estimate, degree - 1)) / degree as f64;
    }
    estimate
}

fn decoder_params(granularity: i32, mux_ratio: i32, driver_width: i32) -> DecoderParams {
    const WRITE_ENABLE_CAP: f64 = 0.000_000_000_000_012_054_7;
    const INVERTER_INPUT_CAP: f64 = 0.000_000_000_000_004_482_092_764_998_187;
    const NAND_TO_INV_RESISTANCE: f64 = 1_478.364_147_093_855 / 1_422.118_502_462_849;

    let fanout =
        granularity as f64 * WRITE_ENABLE_CAP * (driver_width as f64 / 3000.0) / INVERTER_INPUT_CAP;
    let buffer_inverters = if fanout < 27.0 {
        2
    } else if fanout < 243.0 {
        4
    } else {
        6
    };
    let effort = nth_root(NAND_TO_INV_RESISTANCE * fanout, buffer_inverters + 1);
    // DecoderStagePhysicalDesignScript caps electrical folding by the width
    // available to one write-mask group. These are the minimum-style decoder
    // pitch/tap values and SRAM bitcell/tap pitches used by SRAM22.
    let wmask_unit_width = granularity * (1200 * mux_ratio + 1300);
    let max_width = wmask_unit_width - 380;
    let group_pitch = 4 * 1580 + 1000;
    let taps = ceil_div(max_width, group_pitch) + 1;
    let folding_limit = ((max_width - 1000 * taps) / 1580).max(1);
    let stages = (0..buffer_inverters + 2)
        .map(|stage| {
            let scale = powi(effort, stage.min(buffer_inverters)) / NAND_TO_INV_RESISTANCE;
            let base_n = if stage == 0 {
                2000
            } else {
                scale_width(1000, scale, 500)
            };
            let base_p = if stage == 0 {
                2500
            } else {
                scale_width(2500, scale, 1250)
            };
            let folds = (base_n.min(base_p) / 1800).max(1).min(folding_limit);
            DecoderStageParams {
                nwidth: round_positive(base_n as f64 / folds as f64 / 10.0) * 10,
                pwidth: round_positive(base_p as f64 / folds as f64 / 10.0) * 10,
                folds,
            }
        })
        .collect::<Vec<_>>();
    let max_folds = stages.last().expect("decoder has stages").folds;
    DecoderParams { stages, max_folds }
}

fn replica_precharge_outline_top(p: PrechargeParams) -> i32 {
    p.equalizer_width + 2 * p.pull_up_width + 1980 + p.en_b_width + 400
}

fn replica_unit_width(target: i32, columns: i32) -> i32 {
    snap_50((target / columns).max(800))
}

fn replica_column_params(
    p: SramParams,
    col: &ColParams,
    replica_precharge: PrechargeParams,
) -> ReplicaColumnParams {
    let gate_target = snap_50(ceil_div(3360, 6));
    let drain_n_target = snap_50(ceil_div(
        col.mux.nwidth * (p.mux_ratio + 1) + col.wrdriver.nwidth_driver,
        6,
    ));
    let drain_p_target = snap_50(ceil_div(
        col.mux.pwidth * (p.mux_ratio + 1) + col.wrdriver.pwidth_driver,
        6,
    ));
    let max_height =
        replica_precharge_outline_top(replica_precharge) + 2 * replica_precharge.en_b_width + 280;
    let columns = ceil_div(gate_target + drain_n_target + drain_p_target, max_height);
    let gate_width = replica_unit_width(gate_target, columns);
    let drain_n_width = replica_unit_width(drain_n_target, columns);
    let drain_p_width = replica_unit_width(drain_p_target, columns);
    let units = ceil_div(gate_target, gate_width)
        .max(ceil_div(drain_n_target, drain_n_width))
        .max(ceil_div(drain_p_target, drain_p_width));
    ReplicaColumnParams {
        gate_target,
        drain_n_target,
        drain_p_target,
        max_height,
        gate_width,
        drain_n_width,
        drain_p_width,
        units,
    }
}

fn tgate_mux_height(p: TGateMuxParams) -> i32 {
    let nmos_top = 2 * p.pwidth + 2 * p.nwidth + 2330;
    let space = (p.sel_width / 4).max(140);
    let ymin = -(p.sel_width + space) * (p.mux_ratio - 1) - p.sel_width;
    let absolute_top = nmos_top + (p.sel_width + space) * p.mux_ratio + (170 - space).max(0);
    absolute_top - (ymin - 1000)
}

fn replica_column_height(p: ReplicaColumnParams) -> i32 {
    p.gate_width + p.drain_n_width + p.drain_p_width + 2480
}

fn replica_routing_params(
    p: SramParams,
    standalone_col: &ColParams,
    col: &ColParams,
    replica_column: ReplicaColumnParams,
) -> ReplicaRoutingParams {
    let max_height = replica_column
        .max_height
        .max(replica_column_height(replica_column));
    let m0_area = ceil_div(
        standalone_col.wrdriver.pwidth_driver + standalone_col.wrdriver.nwidth_driver,
        6,
    ) * 1080;
    let m1_area = ceil_div(tgate_mux_height(col.mux) * p.mux_ratio, 6) * 1080;
    let m1_width = snap_50((m1_area / max_height).max(320));
    let m0_width = snap_50((m0_area / max_height).max(320));
    let m1_height = snap_50((m1_area / m1_width).max(1000));
    let m0_height = snap_50((m0_area / m0_width).max(1000));
    ReplicaRoutingParams {
        m0_width,
        m0_height,
        m1_width,
        m1_height,
        tracks: m1_width / 320,
    }
}

fn render_gate(out: &mut String, p: PrimitiveGateParams) {
    write!(
        out,
        "PrimitiveGateParams {{ nwidth: {}, pwidth: {}, length: {} }}",
        p.nwidth, p.pwidth, p.length
    )
    .unwrap();
}

fn render_precharge(out: &mut String, p: PrechargeParams) {
    write!(
        out,
        "PrechargeParams {{ length: {}, pull_up_width: {}, equalizer_width: {}, en_b_width: {} }}",
        p.length, p.pull_up_width, p.equalizer_width, p.en_b_width
    )
    .unwrap();
}

fn render_decoder(out: &mut String, p: &DecoderParams) {
    write!(out, "DecoderParams {{ stages: ").unwrap();
    for stage in &p.stages {
        write!(
            out,
            "cons(DecoderStageParams {{ nwidth: {}, pwidth: {}, folds: {} }}, ",
            stage.nwidth, stage.pwidth, stage.folds
        )
        .unwrap();
    }
    out.push_str("[]");
    for _ in &p.stages {
        out.push(')');
    }
    write!(
        out,
        ", num_stages: {}, max_folds: {} }}",
        p.stages.len(),
        p.max_folds
    )
    .unwrap();
}

fn render_col(out: &mut String, p: &ColParams) {
    out.push_str("ColParams {\n            pc: ");
    render_precharge(out, p.pc);
    write!(out, ",\n            mux: TGateMuxParams {{ length: {}, pwidth: {}, nwidth: {}, mux_ratio: {}, idx: {}, sel_width: {} }},", p.mux.length, p.mux.pwidth, p.mux.nwidth, p.mux.mux_ratio, p.mux.idx, p.mux.sel_width).unwrap();
    write!(out, "\n            wrdriver: WriteDriverParams {{ length: {}, pwidth_driver: {}, nwidth_driver: {} }},", p.wrdriver.length, p.wrdriver.pwidth_driver, p.wrdriver.nwidth_driver).unwrap();
    out.push_str("\n            latch: DiffLatchParams { inv_in: ");
    render_gate(out, p.latch.inv_in);
    write!(out, ", lch: {}, inv_out: ", p.latch.lch).unwrap();
    render_gate(out, p.latch.inv_out);
    out.push_str(", invq: ");
    render_gate(out, p.latch.invq);
    write!(out, ", nwidth: {} }},", p.latch.nwidth).unwrap();
    write!(out, "\n            cols: {}, include_wmask: {}, wmask_granularity: {},\n            wmask_decoder: ", p.cols, p.include_wmask, p.wmask_granularity).unwrap();
    render_decoder(out, &p.wmask_decoder);
    out.push_str(",\n        }");
}

/// Render a complete Argon module containing one concrete, pre-sized record.
pub fn render_argon_module(function_name: &str, p: &SramLayoutParams) -> String {
    let mut out = String::from(
        "// Generated by `cargo run -p argon-sram-sizing -- 8 4 64 24`.\n\
         // Electrical and architectural sizing belongs in Rust; this module contains values only.\n\n\
         use lib::params::PrimitiveGateParams;\n\
         use lib::params::DiffLatchParams;\n\
         use lib::params::PrechargeParams;\n\
         use lib::params::TGateMuxParams;\n\
         use lib::params::WriteDriverParams;\n\
         use lib::params::DecoderStageParams;\n\
         use lib::params::DecoderParams;\n\
         use lib::params::ColParams;\n\
         use lib::params::SramParams;\n\
         use lib::params::ReplicaColumnParams;\n\
         use lib::params::ReplicaRoutingParams;\n\
         use lib::params::SramLayoutParams;\n\n",
    );
    writeln!(out, "fn {function_name}() -> SramLayoutParams {{").unwrap();
    writeln!(out, "    SramLayoutParams {{").unwrap();
    writeln!(out, "        sram: SramParams {{ wmask_granularity: {}, mux_ratio: {}, num_words: {}, data_width: {} }},", p.sram.wmask_granularity, p.sram.mux_ratio, p.sram.num_words, p.sram.data_width).unwrap();
    out.push_str("        col: ");
    render_col(&mut out, &p.col);
    out.push_str(",\n        standalone_col: ");
    render_col(&mut out, &p.standalone_col);
    writeln!(out, ",\n        replica_rows: {},", p.replica_rows).unwrap();
    writeln!(out, "        address_dff_count: {},", p.address_dff_count).unwrap();
    out.push_str("        replica_precharge: ");
    render_precharge(&mut out, p.replica_precharge);
    out.push_str(",\n");
    writeln!(out, "        replica_column: ReplicaColumnParams {{ gate_target: {}, drain_n_target: {}, drain_p_target: {}, max_height: {}, gate_width: {}, drain_n_width: {}, drain_p_width: {}, units: {} }},", p.replica_column.gate_target, p.replica_column.drain_n_target, p.replica_column.drain_p_target, p.replica_column.max_height, p.replica_column.gate_width, p.replica_column.drain_n_width, p.replica_column.drain_p_width, p.replica_column.units).unwrap();
    writeln!(out, "        replica_routing: ReplicaRoutingParams {{ m0_width: {}, m0_height: {}, m1_width: {}, m1_height: {}, tracks: {} }},", p.replica_routing.m0_width, p.replica_routing.m0_height, p.replica_routing.m1_width, p.replica_routing.m1_height, p.replica_routing.tracks).unwrap();
    out.push_str("    }\n}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_64x24_values_match_the_original_argon_derivation() {
        let sized = SramParams::new(8, 4, 64, 24).unwrap().size();
        assert_eq!(sized.standalone_col.pc.pull_up_width, 800);
        assert_eq!(sized.standalone_col.mux.pwidth, 1800);
        assert_eq!(sized.standalone_col.wrdriver.pwidth_driver, 1200);
        assert_eq!(sized.col.pc.en_b_width, 1000);
        assert_eq!(sized.col.mux.sel_width, 1000);
        assert_eq!(
            sized.col.wmask_decoder.stages,
            vec![
                DecoderStageParams {
                    nwidth: 2000,
                    pwidth: 2500,
                    folds: 1
                },
                DecoderStageParams {
                    nwidth: 2000,
                    pwidth: 5000,
                    folds: 1
                },
                DecoderStageParams {
                    nwidth: 2080,
                    pwidth: 5180,
                    folds: 2
                },
                DecoderStageParams {
                    nwidth: 2080,
                    pwidth: 5180,
                    folds: 2
                },
            ]
        );
        assert_eq!(sized.replica_rows, 4);
        assert_eq!(sized.address_dff_count, 8);
        assert_eq!(
            sized.replica_column,
            ReplicaColumnParams {
                gate_target: 550,
                drain_n_target: 1200,
                drain_p_target: 1700,
                max_height: 5840,
                gate_width: 800,
                drain_n_width: 1200,
                drain_p_width: 1700,
                units: 1,
            }
        );
        assert_eq!(
            sized.replica_routing,
            ReplicaRoutingParams {
                m0_width: 300,
                m0_height: 1450,
                m1_width: 2200,
                m1_height: 6250,
                tracks: 6,
            }
        );
    }

    #[test]
    fn practical_sizing_space_is_well_formed() {
        for mux_ratio in [4, 8] {
            for granularity in [1, 2, 4, 8] {
                for num_words in [32, 64, 128, 256] {
                    for data_width in [8, 16, 24, 32, 64] {
                        if data_width % granularity != 0 {
                            continue;
                        }
                        let sized = SramParams::new(granularity, mux_ratio, num_words, data_width)
                            .unwrap()
                            .size();
                        let decoder = &sized.col.wmask_decoder;
                        assert!(matches!(decoder.stages.len(), 4 | 6 | 8));
                        assert_eq!(decoder.max_folds, decoder.stages.last().unwrap().folds);
                        assert!(decoder.stages.iter().all(|stage| {
                            stage.nwidth >= 500
                                && stage.pwidth >= 1250
                                && stage.folds >= 1
                                && stage.nwidth % 10 == 0
                                && stage.pwidth % 10 == 0
                        }));
                        assert!(sized.replica_column.units > 0);
                        assert!(sized.replica_routing.tracks > 0);
                    }
                }
            }
        }
    }

    #[test]
    fn checked_in_argon_preset_is_current() {
        let sized = SramParams::new(8, 4, 64, 24).unwrap().size();
        assert_eq!(
            render_argon_module("sram", &sized),
            include_str!("../../../examples/sram/generated.ar")
        );
    }
}
