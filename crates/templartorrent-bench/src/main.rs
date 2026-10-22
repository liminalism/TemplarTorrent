//! Dependency-free, deterministic microbenchmark. JSONL to stdout; never uses the internet.
use std::{hint::black_box, time::Instant};
use templartorrent_core::{
    picker::PieceIndex,
    sequence::{FileSpec, Sequence},
};
fn measurement(name: &str, iterations: usize, mut f: impl FnMut()) {
    for _ in 0..20 {
        f();
    }
    let mut times = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let t = Instant::now();
        f();
        times.push(t.elapsed().as_nanos());
    }
    times.sort_unstable();
    let p = |percent: usize| times[((times.len() - 1) * percent) / 100];
    println!(
        "{{\"benchmark\":\"{name}\",\"iterations\":{iterations},\"p50_ns\":{},\"p95_ns\":{},\"p99_ns\":{},\"max_ns\":{}}}",
        p(50),
        p(95),
        p(99),
        times[times.len() - 1]
    );
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut pieces = 100_000usize;
    let mut iterations = 200usize;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args
            .next()
            .ok_or("expected a value after --pieces or --iterations")?;
        match arg.as_str() {
            "--pieces" => pieces = value.parse()?,
            "--iterations" => iterations = value.parse()?,
            _ => return Err("unknown option".into()),
        }
    }
    if !(1..=2_000_000).contains(&pieces) || !(1..=100_000).contains(&iterations) {
        return Err("benchmark size outside supported limits".into());
    }
    let words = pieces.div_ceil(64);
    let peer = vec![u64::MAX; words];
    let mut have = vec![0; words];
    for (i, w) in have.iter_mut().enumerate() {
        if i % 5 == 0 {
            *w = 0xaaaaaaaaaaaaaaaa;
        }
    }
    let busy = vec![0; words];
    let mut out = vec![0; words];
    println!(
        "{{\"schema\":1,\"pieces\":{pieces},\"backend\":\"{:?}\",\"arch\":\"{}\",\"os\":\"{}\",\"synthetic\":true}}",
        templartorrent_simd::backend(),
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    measurement("eligible_scalar", iterations, || {
        templartorrent_simd::eligible_scalar(
            black_box(&peer),
            black_box(&have),
            black_box(&busy),
            &mut out,
        )
        .unwrap();
        black_box(&out);
    });
    measurement("eligible_auto", iterations, || {
        templartorrent_simd::eligible(
            black_box(&peer),
            black_box(&have),
            black_box(&busy),
            &mut out,
        )
        .unwrap();
        black_box(&out);
    });
    let file = FileSpec {
        path: "Episode 01".into(),
        length: pieces as u64 * 16_384,
        wanted: true,
    };
    let sequence = Sequence::new(vec![file], 16_384, pieces)?;
    let availability: Vec<_> = (0..pieces).map(|i| (i % 51 + 1) as u32).collect();
    measurement("reference_piece_pick", iterations, || {
        black_box(
            sequence
                .pick(black_box(&peer), black_box(&busy), black_box(&availability))
                .unwrap(),
        );
    });
    let mut index = PieceIndex::new(&sequence, &busy, &availability, 0x5eed);
    measurement("indexed_piece_pick", iterations, || {
        black_box(index.pick(
            black_box(&peer),
            black_box(&availability),
            sequence.piece_ranks(),
            sequence.verified_words(),
            black_box(&busy),
        ));
    });
    // A peer holding one piece in sixteen takes the sparse path.
    let sparse: Vec<u64> = (0..words).map(|_| 0x0001_0001_0001_0001).collect();
    measurement("indexed_piece_pick_sparse_peer", iterations, || {
        black_box(index.pick(
            black_box(&sparse),
            black_box(&availability),
            sequence.piece_ranks(),
            sequence.verified_words(),
            black_box(&busy),
        ));
    });
    // Maintenance costs the reference never pays: a full rank rebuild and a
    // whole-bitfield availability sweep (one seeder joining).
    measurement("indexed_rebuild", iterations.min(50), || {
        index.rebuild(&sequence, black_box(&busy), black_box(&availability));
        black_box(&index);
    });
    let mut bumped = availability.clone();
    measurement("indexed_availability_sweep", iterations.min(50), || {
        for (piece, value) in bumped.iter_mut().enumerate() {
            *value += 1;
            index.set_availability(piece, *value);
        }
        black_box(&index);
    });
    Ok(())
}
