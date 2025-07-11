enum Layer {
	Met1,
	Via1,
	Met2,
}

cell vias() {
    let met2 = Rect(Layer::Met2, x0=0, x1=100, y0=0, y1=100)!;
    let met1 = Rect(Layer::Met1, x0=met2.x0 - 5, x1=met2.x1 + 5, y0=0, y1=100)!;
    let via = Rect(Layer::Via1, x0=0, x1=20, y0=0, y1=20);
}
