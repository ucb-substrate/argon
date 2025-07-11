// #![backend(Backend)]

const NWELL: String = "NWELL";
const DIFF: String = "DIFF";
const TAP: String = "TAP";
const PSDM: String = "PSDM";
const NSDM: String = "NSDM";
const POLY: String = "POLY";
const LICON1: String = "LICON1";
const NPC: String = "NPC";
const LI1: String = "LI1";
const TEMP: String = "TEMP";

cell inverter(nw: f64, pw: f64) {
    let ndiff = Rect::new(DIFF, x0=0, y0=0, y1=nw + nw)!;
    let poly = Rect::new(POLY, y0=ndiff.y0-130)!;
    eq(poly.x0 + poly.x1, ndiff.x0 + ndiff.x1);
    eq(poly.x1 - poly.x0, 150);
    eq(ndiff.x0, poly.x0 - 55 - 170 - 130);
    eq(ndiff.x1, poly.x1 + 55 + 170 + 130);
    let nsdm = Rect(NSDM, x0=ndiff.x0-130, x1=ndiff.x1+130, y0=ndiff.y0-130, y1=ndiff.y1+130);
    let vss_s = Rect(LI1, x1=poly.x0-55);
    eq(vss_s.x1 - vss_s.x0, 170);
    eq(vss_s.y1, ndiff.y1 + 100);

    let vss_ptap = Rect(LI1)!;
    eq(vss_ptap.x1 - vss_ptap.x0, 800);
    eq(vss_ptap.y1 - vss_ptap.y0, 800);
    let ptap = Rect(TAP, x0=vss_ptap.x0-40, x1=vss_ptap.x1+40, y0=vss_ptap.y0-65, y1=vss_ptap.y1+65)!;
    let psdm = Rect(PSDM, x0=ptap.x0-130, x1=ptap.x1+130, y0=ptap.y0-130, y1=ptap.y1+130)!;
    eq(nsdm.y0 - psdm.y1, 20);
    eq(nsdm.x0+nsdm.x1, psdm.x0+psdm.x1);
    eq(vss_s.y0, vss_ptap.y0);

    let pdiff = Rect(DIFF, x0=ndiff.x0, x1=ndiff.x1)!;
    eq(pdiff.y1 - pdiff.y0, pw);
    eq(poly.y1, pdiff.y1 + 130);
    let psdm = Rect(PSDM, x0=pdiff.x0-130, x1=pdiff.x1+130, y0=pdiff.y0-130, y1=pdiff.y1+130)!;
    eq(psdm.y0, nsdm.y1 + 170+350+170+60);
    let vdd_s = Rect(LI1, x1=poly.x0-55)!;
    eq(vdd_s.x1 - vdd_s.x0, 170);
    eq(vdd_s.y0, pdiff.y0 - 100);

    let vdd_ntap = Rect(LI1)!;
    eq(vdd_ntap.x1 - vdd_ntap.x0, 800);
    eq(vdd_ntap.y1 - vdd_ntap.y0, 800);
    let ntap = Rect(TAP, x0=vdd_ntap.x0-40, x1=vdd_ntap.x1+40, y0=vdd_ntap.y0-65, y1=vdd_ntap.y1+65)!;
    let nsdm = Rect(NSDM, x0=ntap.x0-130, x1=ntap.x1+130, y0=ntap.y0-130, y1=ntap.y1+130)!;
    eq(nsdm.y0 - psdm.y1, 20);
    eq(nsdm.x0+nsdm.x1, psdm.x0+psdm.x1);
    eq(vdd_s.y1, vdd_ntap.y1);

    let vout = Rect(LI1, x0=poly.x1+55, y0=ndiff.y0-100, y1=pdiff.y1+100)!;
    eq(vout.x1 - vout.x0, 170);

    let poly_gcon = Rect(POLY, x1=poly.x1)!;
    eq(poly_gcon.y1 - poly_gcon.y0, 350);
    eq(poly_gcon.x1 - poly_gcon.x0, 430);
    eq(poly_gcon.y0 + poly_gcon.y1, vss_s.y1 + vdd_s.y0);

    let licon_gate = Rect(LICON1, x0=vss_s.x0, x1=vss_s.x1)!;
    eq(licon_gate.y1 - licon_gate.y0, 170);
    eq(licon_gate.y0 + licon_gate.y1, poly_gcon.y0 + poly_gcon.y1);
    Rect(NPC, x0=licon_gate.x0-100, x1=licon_gate.x1+100, y0=licon_gate.y0-100, y1=licon_gate.y1+100)!;
    let vin = Rect(Li1, x0=licon_gate.x0, x1=licon_gate.x1)!;
    eq(vin.y1 - vin.y0, 380);
    eq(vin.y0 + vin.y1, licon_gate.y0 + licon_gate.y1);

    let nwell = Rect(NWELL, x0=ntap.x0-180, x1=ntap.x1+180, y0=pdiff.y0-180, y1=ntap.y1+180)!;

    let licon = Rect(LICON1, x0=0, x1=170, y0=0, y1=170);
    let cons = Rect(TEMP, x0=vdd_s.x0, x1=vdd_s.x1, y0=pdiff.y0+40, y1=pdiff.y1-40);
    // MaxArray!(licon, cons, 170, 170);
    let cons = Rect(TEMP, x0=vout.x0, x1=vout.x1, y0=pdiff.y0+40, y1=pdiff.y1-40);
    // MaxArray!(licon, cons, 170, 170);
    let cons = Rect(TEMP, x0=vout.x0, x1=vout.x1, y0=ndiff.y0+40, y1=ndiff.y1-40);
    // MaxArray!(licon, cons, 170, 170);
    let cons = Rect(TEMP, x0=vss_s.x0, x1=vss_s.x1, y0=ndiff.y0+40, y1=ndiff.y1-40);
    // MaxArray!(licon, cons, 170, 170);
    let cons = Rect(TEMP, x0=vss_ptap.x0, x1=vss_ptap.x1, y0=vss_ptap.y0+80, y1=vss_ptap.y1-80);
    // MaxArray!(licon, cons, 170, 170);
    let cons = Rect(TEMP, x0=vdd_ntap.x0, x1=vdd_ntap.x1, y0=vdd_ntap.y0+80, y1=vdd_ntap.y1-80);
    // MaxArray!(licon, cons, 170, 170);
}
