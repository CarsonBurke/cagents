//! Run alone on fw16; compare the same optimized build profile and inputs.
use super::*;
use std::time::Instant;

fn thread_cpu() -> f64 {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes only the live timespec.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut value) },
        0
    );
    value.tv_sec as f64 + value.tv_nsec as f64 / 1e9
}

#[test]
#[ignore = "fw16 paragraph construction and plain-stream throughput"]
fn paragraph_storage_throughput() {
    let styled = "A measured **plain** paragraph with `code`.\n\n";
    let styled = styled.repeat((1024 * 1024usize).div_ceil(styled.len()));
    let styled = &styled[..1024 * 1024];
    let plain = "x".repeat(1024 * 1024);
    for workload in ["styled publication", "plain publication", "plain stream"] {
        let mut cpu = Vec::new();
        let mut wall = Vec::new();
        for sample in 0..10 {
            let started = Instant::now();
            let cpu_start = thread_cpu();
            let content = if workload == "plain stream" {
                let mut content = Content::new();
                for chunk in plain.as_bytes().chunks(256) {
                    content.push_str(std::str::from_utf8(chunk).unwrap());
                }
                content
            } else {
                Content::parse(if workload == "styled publication" {
                    styled
                } else {
                    &plain
                })
            };
            let publication = crate::app::chat_ui::prose_document::Data::snapshot(&content);
            std::hint::black_box(&publication);
            drop(publication);
            drop(content);
            let elapsed_cpu = 1000. * (thread_cpu() - cpu_start);
            if sample != 0 {
                cpu.push(elapsed_cpu);
                wall.push(1000. * started.elapsed().as_secs_f64());
            }
        }
        cpu.sort_by(f64::total_cmp);
        wall.sort_by(f64::total_cmp);
        eprintln!(
            "paragraph_storage_cost workload={workload:?} bytes=1048576 samples=9 cpu_median_ms={:.3} cpu_max_ms={:.3} wall_median_ms={:.3}",
            cpu[4], cpu[8], wall[4]
        );
    }
}
