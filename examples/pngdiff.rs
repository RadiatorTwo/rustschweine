//! Compares two pictures (`HOGS_SHOT`s):  pngdiff <a.png> <b.png> [diff.png]
//! Prints how many pixels differ (by more than 8 and 32 in a channel) and where; the
//! optional third file shows the differences, 4 times brighter.
use std::fs::File;

fn load(path: &str) -> (Vec<[u8; 3]>, u32, u32) {
    let mut dec = png::Decoder::new(std::io::BufReader::new(File::open(path).expect(path)));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = dec.read_info().unwrap();
    let mut buf = vec![0; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut buf).unwrap();
    let n = info.color_type.samples();
    let px = buf[..info.buffer_size()].chunks(n).map(|c| [c[0], c[1], c[2]]).collect();
    (px, info.width, info.height)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (a, w, h) = load(&args[1]);
    let (b, w2, h2) = load(&args[2]);
    assert_eq!((w, h), (w2, h2), "sizes differ");
    let (mut small, mut big) = (0, 0);
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    let mut out = Vec::with_capacity(a.len() * 3);
    for (i, (p, q)) in a.iter().zip(&b).enumerate() {
        let d = (0..3).map(|k| p[k].abs_diff(q[k])).max().unwrap();
        if d > 8 {
            small += 1;
            let (x, y) = (i as u32 % w, i as u32 / w);
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
        big += (d > 32) as u32;
        out.extend((0..3).map(|k| p[k].abs_diff(q[k]).saturating_mul(4)));
    }
    println!("{small} pixels differ by more than 8, {big} by more than 32, of {}", a.len());
    if small > 0 {
        println!("within x {x0}..={x1}, y {y0}..={y1}");
    }
    if let Some(path) = args.get(3) {
        let mut enc = png::Encoder::new(File::create(path).unwrap(), w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.write_header().unwrap().write_image_data(&out).unwrap();
    }
}
